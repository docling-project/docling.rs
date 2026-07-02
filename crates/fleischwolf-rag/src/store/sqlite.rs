//! SQLite vector store (feature `sqlite`, on by default).
//!
//! Uses the bundled SQLite that ships with `sqlx`, plus the statically-compiled
//! [`sqlite-vec`](https://github.com/asg017/sqlite-vec) extension: embeddings live
//! in a `vec0` virtual table (`chunks_vec`, cosine metric) keyed by the `chunks`
//! rowid, and `vector_search` is a real KNN `MATCH` query instead of a full-table
//! scan. The extension is registered process-wide with `sqlite3_auto_extension`
//! before the first connection, so every pooled connection sees `vec0`.

use super::VectorStore;
use crate::math;
use crate::model::{Chunk, Document, Scored};
use crate::{RagError, Result};
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::str::FromStr;
use std::sync::Once;

/// Register sqlite-vec for every SQLite connection opened by this process.
/// Safe to call repeatedly; the registration itself happens once.
fn register_sqlite_vec() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        // sqlite3_auto_extension expects `fn()`; sqlite3_vec_init's real signature
        // (db, err, api) is what SQLite actually calls it with.
        #[allow(clippy::missing_transmute_annotations)]
        libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// SQLite-backed store with sqlite-vec KNN search.
pub struct SqliteStore {
    pool: SqlitePool,
    dim: usize,
}

impl SqliteStore {
    /// Connect to (creating if missing) the SQLite database at `url`
    /// (e.g. `sqlite://data/rag.db` or `sqlite::memory:`), expecting
    /// `dim`-dimensional embeddings.
    pub async fn connect(url: &str, dim: usize) -> Result<Self> {
        register_sqlite_vec();
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(|e| RagError::config(format!("invalid RAG_DATABASE_URL '{url}': {e}")))?
            .create_if_missing(true);
        // SQLite won't create parent directories for the DB file; do it ourselves.
        let filename = opts.get_filename();
        if let Some(parent) = filename.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        Ok(SqliteStore { pool, dim })
    }
}

fn row_to_chunk(row: &sqlx::sqlite::SqliteRow) -> Result<Chunk> {
    let metadata: String = row.try_get("metadata")?;
    Ok(Chunk {
        id: row.try_get("id")?,
        doc_id: row.try_get("doc_id")?,
        ordinal: row.try_get("ordinal")?,
        text: row.try_get("text")?,
        token_count: row.try_get("token_count")?,
        metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
        embedding: None,
    })
}

#[async_trait]
impl VectorStore for SqliteStore {
    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS documents (
                id TEXT PRIMARY KEY,
                source_uri TEXT NOT NULL,
                title TEXT NOT NULL,
                hash TEXT NOT NULL,
                metadata TEXT NOT NULL DEFAULT 'null',
                created_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS chunks (
                id TEXT PRIMARY KEY,
                doc_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                text TEXT NOT NULL,
                token_count INTEGER NOT NULL,
                metadata TEXT NOT NULL DEFAULT 'null'
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id)")
            .execute(&self.pool)
            .await?;
        // The vec0 virtual table holds one embedding per chunk, sharing the chunks
        // table's rowid. Dimension and metric are fixed at creation time.
        sqlx::query(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
                embedding float[{}] distance_metric=cosine
            )",
            self.dim
        ))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn upsert_document(&self, doc: &Document) -> Result<()> {
        sqlx::query(
            "INSERT INTO documents (id, source_uri, title, hash, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                source_uri = excluded.source_uri,
                title = excluded.title,
                hash = excluded.hash,
                metadata = excluded.metadata",
        )
        .bind(&doc.id)
        .bind(&doc.source_uri)
        .bind(&doc.title)
        .bind(&doc.hash)
        .bind(serde_json::to_string(&doc.metadata)?)
        .bind(&doc.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_document_by_hash(&self, hash: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM documents WHERE hash = ?1 LIMIT 1")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("id")))
    }

    async fn insert_chunks(&self, chunks: &[Chunk]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for c in chunks {
            let emb = c
                .embedding
                .as_ref()
                .ok_or_else(|| RagError::Store(format!("chunk {} has no embedding", c.id)))?;
            if emb.len() != self.dim {
                return Err(RagError::Store(format!(
                    "chunk {} embedding has dim {}, store expects {}",
                    c.id,
                    emb.len(),
                    self.dim
                )));
            }
            let rowid: i64 = sqlx::query_scalar(
                "INSERT INTO chunks (id, doc_id, ordinal, text, token_count, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING rowid",
            )
            .bind(&c.id)
            .bind(&c.doc_id)
            .bind(c.ordinal)
            .bind(&c.text)
            .bind(c.token_count)
            .bind(serde_json::to_string(&c.metadata)?)
            .fetch_one(&mut *tx)
            .await?;
            // vec0 accepts a raw little-endian f32 blob as the vector value.
            sqlx::query("INSERT INTO chunks_vec (rowid, embedding) VALUES (?1, ?2)")
                .bind(rowid)
                .bind(math::to_bytes(emb))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn vector_search(&self, query: &[f32], k: usize) -> Result<Vec<Scored>> {
        if query.len() != self.dim {
            return Err(RagError::Store(format!(
                "query embedding has dim {}, store expects {}",
                query.len(),
                self.dim
            )));
        }
        // KNN via the vec0 MATCH operator; distance is cosine distance in [0, 2],
        // so similarity = 1 - distance.
        let rows = sqlx::query(
            "SELECT c.id, c.doc_id, c.ordinal, c.text, c.token_count, c.metadata, v.distance
             FROM (SELECT rowid, distance FROM chunks_vec
                   WHERE embedding MATCH ?1 AND k = ?2) v
             JOIN chunks c ON c.rowid = v.rowid
             ORDER BY v.distance",
        )
        .bind(math::to_bytes(query))
        .bind(k as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let distance: f64 = row.try_get("distance")?;
            out.push(Scored::new(row_to_chunk(row)?, 1.0 - distance as f32));
        }
        Ok(out)
    }

    async fn all_chunks(&self) -> Result<Vec<Chunk>> {
        let rows =
            sqlx::query("SELECT id, doc_id, ordinal, text, token_count, metadata FROM chunks")
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(row_to_chunk).collect()
    }

    async fn count_chunks(&self) -> Result<usize> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM chunks")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n") as usize)
    }

    async fn count_documents(&self) -> Result<usize> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM documents")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n") as usize)
    }

    async fn list_documents(&self) -> Result<Vec<Document>> {
        let rows =
            sqlx::query("SELECT id, source_uri, title, hash, metadata, created_at FROM documents")
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|row| {
                let metadata: String = row.try_get("metadata")?;
                Ok(Document {
                    id: row.try_get("id")?,
                    source_uri: row.try_get("source_uri")?,
                    title: row.try_get("title")?,
                    hash: row.try_get("hash")?,
                    metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    async fn clear(&self) -> Result<()> {
        sqlx::query("DELETE FROM chunks_vec")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM chunks")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM documents")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{Embedder, HashEmbedder};
    use crate::model::Document;

    #[tokio::test]
    async fn knn_search_via_sqlite_vec() {
        let store = SqliteStore::connect("sqlite::memory:", 64).await.unwrap();
        store.migrate().await.unwrap();

        let embedder = HashEmbedder::new(64);
        let doc = Document::new("mem://t", "T", "h1");
        store.upsert_document(&doc).await.unwrap();

        let texts = [
            "vector database semantic search",
            "banana smoothie recipe",
            "tokio async runtime",
        ];
        let mut chunks = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            let mut c = Chunk::new(&doc.id, i as i64, *t, 0);
            c.embedding = Some(embedder.embed_one(t).await.unwrap());
            chunks.push(c);
        }
        store.insert_chunks(&chunks).await.unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 3);

        let q = embedder
            .embed_one("semantic search in a vector database")
            .await
            .unwrap();
        let hits = store.vector_search(&q, 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].chunk.text.contains("vector database"),
            "got: {}",
            hits[0].chunk.text
        );
        assert!(hits[0].score >= hits[1].score);

        // Dedup lookup and clear.
        assert_eq!(
            store.find_document_by_hash("h1").await.unwrap(),
            Some(doc.id.clone())
        );
        store.clear().await.unwrap();
        assert_eq!(store.count_chunks().await.unwrap(), 0);
        assert!(store.vector_search(&q, 2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_wrong_dimension() {
        let store = SqliteStore::connect("sqlite::memory:", 8).await.unwrap();
        store.migrate().await.unwrap();
        assert!(store.vector_search(&[0.0; 4], 1).await.is_err());
    }
}
