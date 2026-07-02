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
