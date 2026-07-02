//! End-to-end orchestration: ingestion (source → convert → chunk → embed → store)
//! and querying (retrieve → optional LLM answer synthesis).

use crate::chunk::Chunker;
use crate::embed::{self, Embedder};
use crate::llm::{self, ChatModel, Message};
use crate::metrics::{self, ProcessingMetrics, Timings};
use crate::model::{content_hash, Document, RetrievalMode, Scored};
use crate::retrieve::Retriever;
use crate::source::{self, DocumentSource, SourceRef};
use crate::store::{self, VectorStore};
use crate::{RagConfig, RagError, Result};
use fleischwolf::{DocumentConverter, InputFormat, SourceDocument};
use std::sync::Arc;

/// A fully-wired RAG pipeline built from a [`RagConfig`].
#[derive(Clone)]
pub struct Pipeline {
    cfg: RagConfig,
    source: Arc<dyn DocumentSource>,
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
    chat: Option<Arc<dyn ChatModel>>,
    chunker: Chunker,
}

/// What happened to one document during ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Ingested; carries the number of chunks stored.
    Ingested(usize),
    /// Skipped because an identical document (same hash) was already stored.
    Skipped,
}

/// Aggregate ingestion result over a whole source.
#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    pub documents_ingested: usize,
    pub documents_skipped: usize,
    pub documents_failed: usize,
    pub chunks_added: usize,
}

/// A synthesized answer plus the chunks it was grounded in.
#[derive(Debug, Clone)]
pub struct Answer {
    pub text: String,
    pub sources: Vec<Scored>,
}

impl Pipeline {
    /// Build every component from config. The LLM client is created only if
    /// `OPENROUTER_API_KEY` is set (LLM-backed modes error otherwise).
    pub async fn from_config(cfg: &RagConfig) -> Result<Self> {
        let source = source::from_config(cfg)?;
        let embedder = embed::from_config(cfg)?;
        let store = store::from_config(cfg).await?;
        let chat = match cfg.openrouter_api_key {
            Some(_) => Some(llm::from_config(cfg)?),
            None => None,
        };
        let chunker = Chunker::from_config(cfg);
        Ok(Pipeline {
            cfg: cfg.clone(),
            source,
            embedder,
            store,
            chat,
            chunker,
        })
    }

    /// The underlying store (for counts, admin, tests).
    pub fn store(&self) -> &Arc<dyn VectorStore> {
        &self.store
    }

    /// The resolved configuration this pipeline was built from.
    pub fn config(&self) -> &RagConfig {
        &self.cfg
    }

    /// A retriever over this pipeline's store/embedder/LLM.
    pub fn retriever(&self) -> Retriever {
        Retriever::new(self.store.clone(), self.embedder.clone(), self.chat.clone())
            .with_rrf_k(self.cfg.rrf_k)
            .with_multiquery_n(self.cfg.multiquery_n)
    }

    /// Convert raw bytes to `(title, markdown, pages, parse_secs)` using
    /// `fleischwolf`. Runs the sync converter on a blocking thread; `parse_secs`
    /// times the conversion itself (page counting excluded).
    async fn to_markdown(
        name: String,
        bytes: Vec<u8>,
    ) -> Result<(String, String, Option<usize>, f64)> {
        tokio::task::spawn_blocking(move || {
            let ext = name.rsplit('.').next().unwrap_or("");
            let fmt = InputFormat::from_extension(ext)
                .ok_or_else(|| RagError::Conversion(format!("unsupported extension '.{ext}'")))?;
            let pages = metrics::count_pages(fmt, &bytes);
            let src = SourceDocument::from_bytes(name.clone(), fmt, bytes);
            let start = std::time::Instant::now();
            let result = DocumentConverter::new()
                .convert(src)
                .map_err(|e| RagError::Conversion(e.to_string()))?;
            let md = result.document.export_to_markdown();
            let parse_secs = start.elapsed().as_secs_f64();
            let title = first_heading(&md).unwrap_or_else(|| stem(&name));
            Ok((title, md, pages, parse_secs))
        })
        .await
        .map_err(|e| RagError::Conversion(format!("convert join: {e}")))?
    }

    /// Ingest a single document reference. Deduplicates on content hash, and
    /// records per-phase processing metrics in the document's JSON metadata.
    pub async fn ingest_ref(&self, r: &SourceRef) -> Result<IngestOutcome> {
        let bytes = self.source.fetch(r).await?;
        let hash = content_hash(&bytes);
        if self.store.find_document_by_hash(&hash).await?.is_some() {
            return Ok(IngestOutcome::Skipped);
        }

        let file_bytes = bytes.len() as u64;
        let (title, markdown, pages, parse_secs) = Self::to_markdown(r.name.clone(), bytes).await?;
        let words = markdown.split_whitespace().count();

        // The document row must exist before its chunks (FK); metrics are filled
        // in with a second upsert once every phase has been timed.
        let mut doc = Document::new(&r.uri, title, &hash)
            .with_metadata(serde_json::json!({ "source": r.uri }));
        self.store.upsert_document(&doc).await?;

        let start = std::time::Instant::now();
        let mut chunks = self.chunker.chunk(&doc.id, &markdown);
        let chunk_secs = start.elapsed().as_secs_f64();

        let mut embed_secs = 0.0;
        let mut embedded_words = 0;
        let n = chunks.len();
        if !chunks.is_empty() {
            let start = std::time::Instant::now();
            self.embed_chunks(&mut chunks).await?;
            embed_secs = start.elapsed().as_secs_f64();
            embedded_words = chunks
                .iter()
                .map(|c| c.text.split_whitespace().count())
                .sum::<usize>();
            self.store.insert_chunks(&chunks).await?;
        }

        let m = ProcessingMetrics::compute(
            file_bytes,
            pages,
            words,
            n,
            embedded_words,
            Timings {
                parse_secs,
                chunk_secs,
                embed_secs,
            },
        );
        tracing::info!(
            uri = %r.uri,
            pages = ?m.pages,
            words = m.words,
            chunks = m.chunks,
            parse_wps = ?m.parsing.words_per_sec,
            embed_wps = ?m.embedding.words_per_sec,
            "ingested document"
        );
        doc.metadata = serde_json::json!({ "source": r.uri, "metrics": m.to_json() });
        self.store.upsert_document(&doc).await?;
        Ok(IngestOutcome::Ingested(n))
    }

    /// Ingest every document the configured source lists.
    pub async fn ingest_all(&self) -> Result<IngestReport> {
        let refs = self.source.list().await?;
        let mut report = IngestReport::default();
        for r in &refs {
            match self.ingest_ref(r).await {
                Ok(IngestOutcome::Ingested(n)) => {
                    report.documents_ingested += 1;
                    report.chunks_added += n;
                }
                Ok(IngestOutcome::Skipped) => report.documents_skipped += 1,
                Err(e) => {
                    report.documents_failed += 1;
                    tracing::warn!(uri = %r.uri, error = %e, "failed to ingest document");
                }
            }
        }
        Ok(report)
    }

    /// Embed chunk texts in batches, filling in each chunk's `embedding`.
    async fn embed_chunks(&self, chunks: &mut [crate::model::Chunk]) -> Result<()> {
        const BATCH: usize = 64;
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let mut embeddings = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            embeddings.extend(self.embedder.embed(batch).await?);
        }
        if embeddings.len() != chunks.len() {
            return Err(RagError::Embedding("embedding count mismatch".into()));
        }
        for (chunk, emb) in chunks.iter_mut().zip(embeddings) {
            chunk.embedding = Some(emb);
        }
        Ok(())
    }

    /// Retrieve the top `k` chunks for a query under `mode`.
    pub async fn query(&self, mode: RetrievalMode, query: &str, k: usize) -> Result<Vec<Scored>> {
        self.retriever().retrieve(mode, query, k).await
    }

    /// Retrieve, then ask the LLM to answer grounded in the retrieved chunks.
    pub async fn answer(&self, query: &str, mode: RetrievalMode, k: usize) -> Result<Answer> {
        let chat = self.chat.as_ref().ok_or_else(|| {
            RagError::Llm("answering needs an LLM; set OPENROUTER_API_KEY".into())
        })?;
        let hits = self.query(mode, query, k).await?;
        let context = hits
            .iter()
            .enumerate()
            .map(|(i, h)| format!("[{}] {}", i + 1, h.chunk.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        let system = "Answer the user's question using only the provided context passages. \
                      Cite the passage numbers you used like [1]. If the context does not \
                      contain the answer, say so.";
        let user = format!("Context:\n{context}\n\nQuestion: {query}");
        let text = chat
            .complete(&[Message::system(system), Message::user(&user)])
            .await?;
        Ok(Answer {
            text,
            sources: hits,
        })
    }
}

/// First `# `/`## ` heading text in a Markdown string.
fn first_heading(md: &str) -> Option<String> {
    for line in md.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('#') {
            let heading = rest.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }
    None
}

/// File stem of a name (`report.md` → `report`).
fn stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(base)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_and_stem() {
        assert_eq!(
            first_heading("intro\n# Real Title\nbody"),
            Some("Real Title".into())
        );
        assert_eq!(first_heading("no headings here"), None);
        assert_eq!(stem("/a/b/report.md"), "report");
        assert_eq!(stem("noext"), "noext");
    }
}
