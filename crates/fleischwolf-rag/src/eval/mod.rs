//! Evaluation harness: sweep a matrix of `{chunk_size, overlap, retrieval mode}`
//! over a labelled dataset and rank the configurations by retrieval quality.
//!
//! Each `(chunk_size, overlap)` builds a fresh in-memory index (chunk → embed →
//! store) from the dataset's Markdown documents; then every retrieval mode is run
//! against every query and scored with [`metrics`]. Runs fully offline with the
//! hashing embedder and no LLM (the LLM-backed modes are included only when a
//! [`ChatModel`] is supplied).

pub mod metrics;

use crate::chunk::Chunker;
use crate::config::ChunkUnit;
use crate::embed::Embedder;
use crate::llm::ChatModel;
use crate::model::{Chunk, Document, RetrievalMode};
use crate::retrieve::Retriever;
use crate::store::memory::MemoryStore;
use crate::store::VectorStore;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

/// A dataset document, supplied as already-converted Markdown for determinism.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDoc {
    pub name: String,
    pub markdown: String,
}

/// A labelled query. A retrieved chunk is relevant if it contains any `relevant`
/// substring (case-insensitive).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryCase {
    pub query: String,
    pub relevant: Vec<String>,
}

/// A question as loaded from an external questions file. Accepts this crate's
/// `{query, relevant}` shape, the QA-benchmark `{text, kind}` shape, and the
/// output of the `answers` subcommand (`{question, answer, …}` — extra fields
/// are ignored, so `answers --json` output round-trips as a questions file).
/// Only entries with non-empty `relevant` labels can score retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    /// The question text (`text` and `question` accepted as aliases).
    #[serde(alias = "text", alias = "question")]
    pub query: String,
    /// Expected answer kind (`boolean`, `number`, `name`, …) — informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Ground-truth substrings for retrieval scoring (may be empty).
    #[serde(default)]
    pub relevant: Vec<String>,
}

/// Load a questions file (a JSON array of [`Question`]s in either shape).
pub fn load_questions(path: &std::path::Path) -> Result<Vec<Question>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Build eval documents from a directory tree of Markdown files — typically the
/// `RAG_DOCUMENTS_OUTPUT` mirror produced by `ingest`. Non-`.md` files are
/// skipped; `name` is the path relative to `dir`.
pub fn documents_from_md_dir(dir: &std::path::Path) -> Result<Vec<EvalDoc>> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<EvalDoc>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                if let Ok(markdown) = std::fs::read_to_string(&path) {
                    out.push(EvalDoc {
                        name: path
                            .strip_prefix(root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned(),
                        markdown,
                    });
                }
            }
        }
    }
    let mut docs = Vec::new();
    walk(dir, dir, &mut docs);
    if docs.is_empty() {
        return Err(crate::RagError::config(format!(
            "no .md files found under {}",
            dir.display()
        )));
    }
    Ok(docs)
}

/// A full evaluation dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDataset {
    pub documents: Vec<EvalDoc>,
    pub queries: Vec<QueryCase>,
}

/// One cell of the sweep: a chunking config paired with a retrieval mode.
#[derive(Debug, Clone, Serialize)]
pub struct EvalRow {
    pub chunk_size: usize,
    pub overlap: f32,
    pub mode: String,
    pub embedder: String,
    pub recall: f32,
    pub mrr: f32,
    pub ndcg: f32,
    pub avg_latency_ms: f64,
    pub queries: usize,
}

/// The ranked results of a sweep.
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub rows: Vec<EvalRow>,
}

/// Runs sweeps against a dataset using a fixed embedder (+ optional LLM).
pub struct Harness {
    embedder: Arc<dyn Embedder>,
    chat: Option<Arc<dyn ChatModel>>,
}

impl Harness {
    /// Build a harness with the embedder used for every config in the sweep.
    pub fn new(embedder: Arc<dyn Embedder>, chat: Option<Arc<dyn ChatModel>>) -> Self {
        Harness { embedder, chat }
    }

    /// Run the sweep. `modes` that need an LLM are skipped when no [`ChatModel`]
    /// was provided.
    pub async fn run(
        &self,
        dataset: &EvalDataset,
        chunk_configs: &[(usize, f32)],
        modes: &[RetrievalMode],
        top_k: usize,
    ) -> Result<EvalReport> {
        let mut rows = Vec::new();
        for &(size, overlap) in chunk_configs {
            let store = self.build_index(dataset, size, overlap).await?;
            let retriever = Retriever::new(store, self.embedder.clone(), self.chat.clone());
            for &mode in modes {
                if mode.needs_llm() && self.chat.is_none() {
                    continue;
                }
                rows.push(
                    self.score_mode(&retriever, dataset, mode, size, overlap, top_k)
                        .await?,
                );
            }
        }
        // Rank best-first by nDCG, then recall, then MRR.
        rows.sort_by(|a, b| {
            b.ndcg
                .partial_cmp(&a.ndcg)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    b.recall
                        .partial_cmp(&a.recall)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(
                    b.mrr
                        .partial_cmp(&a.mrr)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        Ok(EvalReport { rows })
    }

    /// Chunk + embed + store every dataset document under one chunking config.
    async fn build_index(
        &self,
        dataset: &EvalDataset,
        size: usize,
        overlap: f32,
    ) -> Result<Arc<dyn VectorStore>> {
        let store: Arc<dyn VectorStore> = Arc::new(MemoryStore::new());
        let chunker = Chunker {
            size,
            overlap,
            unit: ChunkUnit::Word,
        };
        for d in &dataset.documents {
            let doc = Document::new(format!("eval://{}", d.name), &d.name, "");
            store.upsert_document(&doc).await?;
            let mut chunks: Vec<Chunk> = chunker.chunk(&doc.id, &d.markdown);
            if chunks.is_empty() {
                continue;
            }
            let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
            let embeddings = self.embedder.embed(&texts).await?;
            for (c, e) in chunks.iter_mut().zip(embeddings) {
                c.embedding = Some(e);
            }
            store.insert_chunks(&chunks).await?;
        }
        Ok(store)
    }

    /// Average metrics over all queries for one mode.
    async fn score_mode(
        &self,
        retriever: &Retriever,
        dataset: &EvalDataset,
        mode: RetrievalMode,
        size: usize,
        overlap: f32,
        top_k: usize,
    ) -> Result<EvalRow> {
        let (mut recall, mut mrr, mut ndcg, mut latency) = (0.0, 0.0, 0.0, 0.0);
        let n = dataset.queries.len().max(1) as f32;
        for q in &dataset.queries {
            let start = Instant::now();
            let hits = retriever.retrieve(mode, &q.query, top_k).await?;
            latency += start.elapsed().as_secs_f64() * 1000.0;
            let m = metrics::evaluate(&hits, &q.relevant, top_k);
            recall += m.recall;
            mrr += m.mrr;
            ndcg += m.ndcg;
        }
        Ok(EvalRow {
            chunk_size: size,
            overlap,
            mode: mode.to_string(),
            embedder: self.embedder.id().to_string(),
            recall: recall / n,
            mrr: mrr / n,
            ndcg: ndcg / n,
            avg_latency_ms: latency / n as f64,
            queries: dataset.queries.len(),
        })
    }
}

impl EvalReport {
    /// Render the report as a Markdown table, best config first.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from(
            "| chunk | overlap | mode | embedder | recall | MRR | nDCG | ms/query |\n\
             |------:|--------:|------|----------|-------:|----:|-----:|---------:|\n",
        );
        for r in &self.rows {
            out.push_str(&format!(
                "| {} | {:.2} | {} | {} | {:.3} | {:.3} | {:.3} | {:.2} |\n",
                r.chunk_size,
                r.overlap,
                r.mode,
                r.embedder,
                r.recall,
                r.mrr,
                r.ndcg,
                r.avg_latency_ms
            ));
        }
        out
    }

    /// Render the report as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;

    fn dataset() -> EvalDataset {
        EvalDataset {
            documents: vec![
                EvalDoc {
                    name: "chunking".into(),
                    markdown: "# Chunking\n\nDocuments are split into overlapping chunks of a \
                               configurable size before embedding for semantic search."
                        .into(),
                },
                EvalDoc {
                    name: "cooking".into(),
                    markdown: "# Cooking\n\nBlend banana yogurt and honey to make a smoothie."
                        .into(),
                },
            ],
            queries: vec![QueryCase {
                query: "how are documents split for embedding".into(),
                relevant: vec!["overlapping chunks".into()],
            }],
        }
    }

    #[test]
    fn loads_questions_in_both_shapes() {
        let dir = std::env::temp_dir().join(format!("rag-q-{}", crate::model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("questions.json");
        std::fs::write(
            &path,
            r#"[
                {"text": "Did X mention mergers?", "kind": "boolean"},
                {"query": "chunk size default", "relevant": ["300 words"]},
                {"question": "answers output row?", "answer": "yes", "sources": 10, "ms": 12.5, "mode": "hybrid"}
            ]"#,
        )
        .unwrap();
        let qs = load_questions(&path).unwrap();
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0].query, "Did X mention mergers?");
        assert_eq!(qs[0].kind.as_deref(), Some("boolean"));
        assert!(qs[0].relevant.is_empty());
        assert_eq!(qs[1].relevant, vec!["300 words"]);
        // The `answers --json` output round-trips: `question` alias, extra
        // fields ignored.
        assert_eq!(qs[2].query, "answers output row?");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn builds_documents_from_md_dir() {
        let dir = std::env::temp_dir().join(format!("rag-mdd-{}", crate::model::new_id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.pdf.md"), "# A\n\nalpha").unwrap();
        std::fs::write(dir.join("sub/b.md"), "# B\n\nbeta").unwrap();
        std::fs::write(dir.join("ignore.txt"), "not markdown").unwrap();
        let docs = documents_from_md_dir(&dir).unwrap();
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().any(|d| d.name == "a.pdf.md"));
        assert!(docs
            .iter()
            .any(|d| d.name.ends_with("b.md") && d.markdown.contains("beta")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn sweep_produces_ranked_rows() {
        let harness = Harness::new(Arc::new(HashEmbedder::new(512)), None);
        let report = harness
            .run(
                &dataset(),
                &[(20, 0.0), (40, 0.1)],
                &RetrievalMode::OFFLINE,
                3,
            )
            .await
            .unwrap();
        // 2 chunk configs x 3 offline modes = 6 rows.
        assert_eq!(report.rows.len(), 6);
        // At least one config retrieves the relevant chunk.
        assert!(report.rows.iter().any(|r| r.recall > 0.0));
        // Markdown/JSON render without panicking.
        assert!(report.to_markdown().contains("nDCG"));
        assert!(report.to_json().contains("recall"));
    }
}
