//! End-to-end pipeline test, fully offline: memory store + hashing embedder + a
//! temp folder of Markdown files. Exercises ingest → dedup → query across modes.

use docling_rag::config::{ChunkUnit, DbBackend, EmbedProvider, QueueKind, SourceKind};
use docling_rag::model::RetrievalMode;
use docling_rag::{Pipeline, RagConfig};

fn offline_config(dir: &std::path::Path) -> RagConfig {
    RagConfig {
        db_backend: DbBackend::Memory,
        embed_provider: EmbedProvider::Hash,
        embed_dim: 512,
        source: SourceKind::Folder,
        source_path: dir.display().to_string(),
        queue: QueueKind::Memory,
        chunk_size: 40,
        chunk_overlap: 0.1,
        chunk_unit: ChunkUnit::Word,
        ..RagConfig::default()
    }
}

#[tokio::test]
async fn ingest_then_retrieve_offline() {
    let dir = std::env::temp_dir().join(format!("rag-it-{}", uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("vectors.md"),
        "# Vector Search\n\nA vector database stores embeddings so that semantic \
         search can find passages by meaning rather than exact keywords.",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("recipes")).unwrap();
    std::fs::write(
        dir.join("recipes/smoothie.md"),
        "# Smoothie\n\nBlend a banana with yogurt and honey for a quick breakfast smoothie.",
    )
    .unwrap();

    let out_dir = std::env::temp_dir().join(format!("rag-out-{}", uuid_like()));
    let mut cfg = offline_config(&dir);
    cfg.documents_output = Some(out_dir.display().to_string());
    let pipeline = Pipeline::from_config(&cfg).await.unwrap();

    // First ingest stores both documents.
    let report = pipeline.ingest_all().await.unwrap();
    assert_eq!(report.documents_ingested, 2, "both docs ingested");
    assert!(report.chunks_added >= 2);

    // RAG_DOCUMENTS_OUTPUT mirrors the source structure with `.md` appended
    // (also for original .md inputs, which may be reformatted).
    assert!(
        out_dir.join("vectors.md.md").exists(),
        "markdown dump written with .md appended"
    );
    assert!(
        std::fs::read_to_string(out_dir.join("recipes/smoothie.md.md"))
            .unwrap()
            .contains("banana"),
        "subdirectory structure mirrored"
    );
    assert_eq!(pipeline.store().count_documents().await.unwrap(), 2);

    // Processing metrics are recorded in each document's JSON metadata.
    let docs = pipeline.store().list_documents().await.unwrap();
    assert_eq!(docs.len(), 2);
    for d in &docs {
        let m = &d.metadata["metrics"];
        assert!(
            m["file_bytes"].as_u64().unwrap() > 0,
            "file_bytes for {}",
            d.title
        );
        assert!(m["words"].as_u64().unwrap() > 0, "words for {}", d.title);
        assert!(m["chunks"].as_u64().unwrap() > 0, "chunks for {}", d.title);
        assert!(m["embedded_words"].as_u64().unwrap() > 0);
        // All three phases have timings; rates exist when the phase was measurable.
        for phase in ["parsing", "chunking", "embedding"] {
            assert!(
                m[phase]["seconds"].is_number(),
                "{phase} seconds for {}",
                d.title
            );
        }
        // Markdown has no page notion, so no pages / pages_per_sec keys.
        assert!(m.get("pages").is_none());
    }

    // Re-ingest is a no-op thanks to content-hash dedup.
    let again = pipeline.ingest_all().await.unwrap();
    assert_eq!(again.documents_ingested, 0);
    assert_eq!(again.documents_skipped, 2);

    // Every offline retrieval mode surfaces the vector-search passage first.
    for mode in RetrievalMode::OFFLINE {
        let hits = pipeline
            .query(
                mode,
                "semantic search over embeddings in a vector database",
                3,
            )
            .await
            .unwrap();
        assert!(!hits.is_empty(), "{mode} returned no hits");
        assert!(
            hits[0]
                .chunk
                .text
                .to_lowercase()
                .contains("vector database"),
            "{mode} ranked wrong chunk: {}",
            hits[0].chunk.text
        );
    }

    // LLM-backed answering errors cleanly without a key.
    assert!(pipeline
        .answer("q", RetrievalMode::Vector, 3)
        .await
        .is_err());

    std::fs::remove_dir_all(&dir).ok();
}

/// A cheap unique-ish suffix without pulling uuid into the test.
fn uuid_like() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
