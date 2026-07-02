//! Pluggable vector store.
//!
//! A store persists [`Document`]s (metadata) and their [`Chunk`]s (text +
//! embedding) in two tables, and answers dense vector search. Keyword (BM25)
//! search is layered on top in [`crate::retrieve`] using [`VectorStore::all_chunks`],
//! so it works identically across every backend.

pub mod memory;

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

use crate::config::DbBackend;
use crate::model::{Chunk, Document, Scored};
use crate::{RagConfig, Result};
use async_trait::async_trait;
use std::sync::Arc;

/// A document + chunk store with dense vector search.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Create tables/indexes if they do not exist. Safe to call repeatedly.
    async fn migrate(&self) -> Result<()>;

    /// Insert or replace a document row (keyed by `id`).
    async fn upsert_document(&self, doc: &Document) -> Result<()>;

    /// Return the id of an existing document with this content hash, if any.
    /// Used to skip re-ingesting unchanged documents.
    async fn find_document_by_hash(&self, hash: &str) -> Result<Option<String>>;

    /// Bulk-insert chunks (each must carry a populated `embedding`).
    async fn insert_chunks(&self, chunks: &[Chunk]) -> Result<()>;

    /// Dense search: the `k` chunks whose embeddings are most cosine-similar to
    /// `query`. Returned chunks omit their embedding vector to keep payloads small.
    async fn vector_search(&self, query: &[f32], k: usize) -> Result<Vec<Scored>>;

    /// Every chunk (id, doc_id, ordinal, text, metadata) with `embedding == None`.
    /// Feeds the BM25 keyword index; keep it modest for large corpora.
    async fn all_chunks(&self) -> Result<Vec<Chunk>>;

    /// Total number of stored chunks.
    async fn count_chunks(&self) -> Result<usize>;

    /// Total number of stored documents.
    async fn count_documents(&self) -> Result<usize>;

    /// Every stored document with its metadata (including processing metrics).
    async fn list_documents(&self) -> Result<Vec<Document>>;

    /// Remove all documents and chunks.
    async fn clear(&self) -> Result<()>;
}

/// Build and connect the store selected by `cfg.db_backend`, running migrations.
pub async fn from_config(cfg: &RagConfig) -> Result<Arc<dyn VectorStore>> {
    let store: Arc<dyn VectorStore> = match cfg.db_backend {
        DbBackend::Memory => Arc::new(memory::MemoryStore::new()),
        DbBackend::Sqlite => {
            #[cfg(feature = "sqlite")]
            {
                Arc::new(sqlite::SqliteStore::connect(&cfg.database_url, cfg.embed_dim).await?)
            }
            #[cfg(not(feature = "sqlite"))]
            {
                return Err(crate::RagError::FeatureDisabled(
                    "sqlite".into(),
                    "sqlite".into(),
                ));
            }
        }
        DbBackend::Postgres => {
            #[cfg(feature = "postgres")]
            {
                Arc::new(postgres::PostgresStore::connect(&cfg.database_url, cfg.embed_dim).await?)
            }
            #[cfg(not(feature = "postgres"))]
            {
                return Err(crate::RagError::FeatureDisabled(
                    "postgres".into(),
                    "postgres".into(),
                ));
            }
        }
    };
    store.migrate().await?;
    Ok(store)
}

/// Rank `(chunk, embedding)` candidates against a query by cosine similarity and
/// keep the top `k`. Shared by the memory and SQLite brute-force backends.
pub(crate) fn top_k_by_cosine(
    query: &[f32],
    candidates: impl IntoIterator<Item = (Chunk, Vec<f32>)>,
    k: usize,
) -> Vec<Scored> {
    let mut scored: Vec<Scored> = candidates
        .into_iter()
        .map(|(chunk, emb)| Scored::new(chunk, crate::math::cosine(query, &emb)))
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(k);
    scored
}
