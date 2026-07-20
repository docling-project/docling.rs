//! Real-model test for the local ONNX embedder (feature `onnx-embed`).
//!
//! Needs the bge-m3 export on disk (`scripts/install/download_dependencies.sh
//! --embed`, ~2.3 GB) — skips with a note when it is absent, so plain CI runs
//! stay green without the download.

#![cfg(feature = "onnx-embed")]

use docling_rag::config::EmbedProvider;
use docling_rag::{embed, math, RagConfig};

#[tokio::test]
async fn bge_m3_embeds_and_ranks_by_meaning() {
    let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let model = format!("{repo}/models/embed/bge-m3.onnx");
    let tokenizer = format!("{repo}/models/embed/tokenizer.json");
    if !std::path::Path::new(&model).exists() {
        eprintln!("skipping: {model} not present (run download_dependencies.sh --embed)");
        return;
    }

    let cfg = RagConfig {
        embed_provider: EmbedProvider::Onnx,
        embed_onnx_path: model,
        embed_tokenizer_path: tokenizer,
        embed_dim: 1024,
        ..RagConfig::default()
    };
    let embedder = embed::from_config(&cfg).unwrap();

    let texts = vec![
        "A vector database stores embeddings for semantic search.".to_string(),
        "Semantic retrieval finds passages by meaning, not keywords.".to_string(),
        "Blend a banana with yogurt for a quick breakfast smoothie.".to_string(),
    ];
    let vecs = embedder.embed(&texts).await.unwrap();
    assert_eq!(vecs.len(), 3);
    for v in &vecs {
        assert_eq!(v.len(), 1024);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "L2-normalized, got {norm}");
    }
    // The two retrieval sentences must sit closer to each other than either
    // does to the smoothie recipe.
    let related = math::cosine(&vecs[0], &vecs[1]);
    let unrelated = math::cosine(&vecs[0], &vecs[2]).max(math::cosine(&vecs[1], &vecs[2]));
    assert!(
        related > unrelated + 0.1,
        "semantic ranking broken: related={related:.3} unrelated={unrelated:.3}"
    );
}
