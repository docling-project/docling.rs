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
use docling::{DocumentConverter, InputFormat, SourceDocument};
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

/// What the overlapped parse/chunk/embed stages produced for one document.
/// Phase seconds are busy time (the stages overlap on the wall clock).
struct StagedOutcome {
    pages: Option<usize>,
    parse_secs: f64,
    chunk_secs: f64,
    embed_secs: f64,
    embedded_words: usize,
    chunks: usize,
    markdown: String,
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

    /// Ingest a single document reference. Deduplicates on content hash, and
    /// records per-phase processing metrics in the document's JSON metadata.
    ///
    /// Processing is **streaming**: `docling.rs`'s `convert_streaming` emits
    /// Markdown as it is produced (per page for PDF), and chunking + embedding
    /// run concurrently on the pieces — parsing of page N overlaps embedding of
    /// pages < N. Phase timings measure busy time, so throughput metrics stay
    /// meaningful even though the phases overlap on the wall clock.
    pub async fn ingest_ref(&self, r: &SourceRef) -> Result<IngestOutcome> {
        let bytes = self.source.fetch(r).await?;
        let hash = content_hash(&bytes);
        if self.store.find_document_by_hash(&hash).await?.is_some() {
            tracing::debug!(uri = %r.uri, "skipping unchanged document");
            return Ok(IngestOutcome::Skipped);
        }
        let file_bytes = bytes.len() as u64;
        tracing::info!(
            uri = %r.uri,
            name = %r.name,
            bytes = file_bytes,
            "processing document"
        );

        // Remove stale rows for this source first: leftovers from interrupted
        // runs, or previous versions of a file whose content changed.
        self.store.delete_documents_by_source(&r.uri).await?;

        // The document row must exist before its chunks (FK). It is inserted
        // with a sentinel hash — the real hash is written only on success, so an
        // interrupted run can never satisfy the dedup check above and the
        // document is reprocessed next time. Title is refined (first heading)
        // and metrics attached with the final upsert.
        let mut doc = Document::new(&r.uri, stem(&r.name), format!("pending:{hash}"))
            .with_metadata(serde_json::json!({ "source": r.uri }));
        self.store.upsert_document(&doc).await?;

        // Run the staged pipeline; on failure roll back the document row and any
        // partially-inserted chunks so a retry reprocesses from scratch instead
        // of being skipped by the hash dedup.
        let staged = self.ingest_streaming(r, &doc.id, bytes).await;
        let out = match staged {
            Ok(out) => out,
            Err(e) => {
                if let Err(del) = self.store.delete_document(&doc.id).await {
                    tracing::warn!(uri = %r.uri, error = %del, "rollback of failed ingest also failed");
                }
                return Err(e);
            }
        };
        let StagedOutcome {
            pages,
            parse_secs,
            chunk_secs,
            embed_secs,
            embedded_words,
            chunks: n,
            markdown,
        } = out;

        let words = markdown.split_whitespace().count();
        let title = first_heading(&markdown).unwrap_or_else(|| stem(&r.name));

        // Optional local FS mirror of the parsed documents (RAG_DOCUMENTS_OUTPUT):
        // same directory structure as the source, `.md` appended to every name
        // (also for original .md inputs — conversion may reformat them).
        // Best-effort: a failed write never fails ingest.
        if let Some(dir) = &self.cfg.documents_output {
            if let Err(e) = dump_markdown(dir, &r.rel_path, &markdown).await {
                tracing::warn!(uri = %r.uri, error = %e, "failed to write markdown dump");
            }
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
        doc.title = title;
        doc.hash = hash; // success: replace the sentinel with the real hash
        doc.metadata = serde_json::json!({ "source": r.uri, "metrics": m.to_json() });
        self.store.upsert_document(&doc).await?;
        Ok(IngestOutcome::Ingested(n))
    }

    /// The overlapped parse → chunk → embed/insert stages for one document.
    async fn ingest_streaming(
        &self,
        r: &SourceRef,
        doc_id: &str,
        bytes: Vec<u8>,
    ) -> Result<StagedOutcome> {
        // --- Stage 1: parser thread. Streams Markdown pieces as converted.
        // Bounded channel: a slow consumer applies backpressure to the converter.
        let (md_tx, mut md_rx) = tokio::sync::mpsc::channel::<String>(16);
        let name = r.name.clone();
        let parser = tokio::task::spawn_blocking(move || -> Result<(Option<usize>, f64)> {
            let ext = name.rsplit('.').next().unwrap_or("");
            let fmt = InputFormat::from_extension(ext)
                .ok_or_else(|| RagError::Conversion(format!("unsupported extension '.{ext}'")))?;
            let pages = metrics::count_pages(fmt, &bytes);
            let src = SourceDocument::from_bytes(name, fmt, bytes);
            let mut stream = DocumentConverter::new()
                .convert_streaming(src)
                .map_err(|e| RagError::Conversion(e.to_string()))?;
            // parse_secs counts time inside the converter only, not time blocked
            // on a full channel (that would bill consumer slowness to parsing).
            let mut parse_secs = 0.0;
            loop {
                let t = std::time::Instant::now();
                let item = stream.next();
                parse_secs += t.elapsed().as_secs_f64();
                match item {
                    Some(Ok(piece)) => {
                        if md_tx.blocking_send(piece).is_err() {
                            break; // consumer failed; its error wins
                        }
                    }
                    Some(Err(e)) => return Err(RagError::Conversion(e.to_string())),
                    None => break,
                }
            }
            Ok((pages, parse_secs))
        });

        // --- Stage 2: incremental chunking; completed chunks go to the embedder.
        // --- Stage 3: embed + insert worker, concurrent with stages 1 and 2.
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Vec<crate::model::Chunk>>(4);
        let embedder = self.embedder.clone();
        let store = self.store.clone();
        let embed_worker = tokio::spawn(async move {
            let mut rx = chunk_rx;
            let (mut embed_secs, mut embedded_words, mut n_chunks) = (0.0f64, 0usize, 0usize);
            while let Some(mut batch) = rx.recv().await {
                let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
                let t = std::time::Instant::now();
                let embeddings = embedder.embed(&texts).await?;
                embed_secs += t.elapsed().as_secs_f64();
                if embeddings.len() != batch.len() {
                    return Err(RagError::Embedding("embedding count mismatch".into()));
                }
                for (chunk, emb) in batch.iter_mut().zip(embeddings) {
                    chunk.embedding = Some(emb);
                }
                embedded_words += texts
                    .iter()
                    .map(|t| t.split_whitespace().count())
                    .sum::<usize>();
                n_chunks += batch.len();
                store.insert_chunks(&batch).await?;
            }
            Ok::<_, RagError>((embed_secs, embedded_words, n_chunks))
        });

        let mut streaming = self.chunker.streaming(doc_id);
        let mut markdown = String::new();
        let mut chunk_secs = 0.0f64;
        let mut backlog: Vec<crate::model::Chunk> = Vec::new();
        const BATCH: usize = 64;
        let mut consume_failed = false;
        while let Some(piece) = md_rx.recv().await {
            let t = std::time::Instant::now();
            let ready = streaming.push(&piece);
            chunk_secs += t.elapsed().as_secs_f64();
            markdown.push_str(&piece);
            backlog.extend(ready);
            while backlog.len() >= BATCH {
                let batch: Vec<_> = backlog.drain(..BATCH).collect();
                if chunk_tx.send(batch).await.is_err() {
                    consume_failed = true; // embed worker died; surface its error
                    break;
                }
            }
            if consume_failed {
                break;
            }
        }
        // Drain: remaining markdown lands in a final section, then flush backlog.
        drop(md_rx);
        if !consume_failed {
            let t = std::time::Instant::now();
            backlog.extend(streaming.finish());
            chunk_secs += t.elapsed().as_secs_f64();
            for batch in backlog.chunks(BATCH) {
                if chunk_tx.send(batch.to_vec()).await.is_err() {
                    break;
                }
            }
        }
        drop(chunk_tx);

        // Join stages; parser errors (bad document) take precedence.
        let (pages, parse_secs) = parser
            .await
            .map_err(|e| RagError::Conversion(format!("convert join: {e}")))??;
        let (embed_secs, embedded_words, n) = embed_worker
            .await
            .map_err(|e| RagError::Embedding(format!("embed join: {e}")))??;

        Ok(StagedOutcome {
            pages,
            parse_secs,
            chunk_secs,
            embed_secs,
            embedded_words,
            chunks: n,
            markdown,
        })
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

/// Mirror a parsed document into the output folder: `<dir>/<rel_path>.md`, with
/// the source's directory structure preserved and `.md` always appended
/// (`report.pdf` → `report.pdf.md`, `notes.md` → `notes.md.md`).
async fn dump_markdown(dir: &str, rel_path: &str, markdown: &str) -> Result<()> {
    // Never let a hostile rel_path escape the output root.
    let rel: std::path::PathBuf = std::path::Path::new(rel_path)
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    let file_name = if rel.as_os_str().is_empty() {
        std::path::PathBuf::from("document")
    } else {
        rel
    };
    let path = std::path::Path::new(dir).join(format!("{}.md", file_name.display()));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, markdown).await?;
    tracing::debug!(path = %path.display(), "wrote markdown dump");
    Ok(())
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
