//! PostgreSQL vector store (feature `postgres`).
//!
//! Uses the `pgvector` extension: embeddings live in a `vector(dim)` column and
//! search is delegated to the `<=>` cosine-distance operator, so it is ANN-ready
//! (add an `ivfflat`/`hnsw` index for scale). Vectors are passed as text literals
//! and cast in SQL, so no extra Rust crate is required.
//!
//! Compile-checked here; exercised only against a live Postgres with `pgvector`.

use super::VectorStore;
use crate::model::{Chunk, Document, Scored};
use crate::{RagError, Result};
use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

/// Postgres-backed store.
pub struct PostgresStore {
    pool: PgPool,
    dim: usize,
}

impl PostgresStore {
    /// Connect to Postgres at `url`, expecting `dim`-dimensional embeddings.
    pub async fn connect(url: &str, dim: usize) -> Result<Self> {
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
        Ok(PostgresStore { pool, dim })
    }
}

/// Render a vector as a pgvector text literal: `[1,2,3]`.
fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

fn row_to_chunk(row: &sqlx::postgres::PgRow) -> Result<Chunk> {
    let metadata: serde_json::Value = row.try_get("metadata")?;
    Ok(Chunk {
        id: row.try_get("id")?,
        doc_id: row.try_get("doc_id")?,
        ordinal: row.try_get("ordinal")?,
        text: row.try_get("text")?,
        token_count: row.try_get("token_count")?,
        metadata,
        embedding: None,
    })
}

#[async_trait]
impl VectorStore for PostgresStore {
    async fn migrate(&self) -> Result<()> {
        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS documents (
                id TEXT PRIMARY KEY,
                source_uri TEXT NOT NULL,
                title TEXT NOT NULL,
                hash TEXT NOT NULL,
                metadata JSONB NOT NULL DEFAULT 'null',
                created_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash)")
            .execute(&self.pool)
            .await?;
        // The vector column dimension is fixed at migrate time.
        sqlx::query(&format!(
            "CREATE TABLE IF NOT EXISTS chunks (
                id TEXT PRIMARY KEY,
                doc_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
                ordinal BIGINT NOT NULL,
                text TEXT NOT NULL,
                token_count BIGINT NOT NULL,
                metadata JSONB NOT NULL DEFAULT 'null',
                embedding vector({}) NOT NULL
            )",
            self.dim
        ))
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn upsert_document(&self, doc: &Document) -> Result<()> {
        sqlx::query(
            "INSERT INTO documents (id, source_uri, title, hash, metadata, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT(id) DO UPDATE SET
                source_uri = EXCLUDED.source_uri,
                title = EXCLUDED.title,
                hash = EXCLUDED.hash,
                metadata = EXCLUDED.metadata",
        )
        .bind(&doc.id)
        .bind(&doc.source_uri)
        .bind(&doc.title)
        .bind(&doc.hash)
        .bind(&doc.metadata)
        .bind(&doc.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_document_by_hash(&self, hash: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM documents WHERE hash = $1 LIMIT 1")
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
            sqlx::query(
                "INSERT INTO chunks (id, doc_id, ordinal, text, token_count, metadata, embedding)
                 VALUES ($1, $2, $3, $4, $5, $6, $7::vector)",
            )
            .bind(&c.id)
            .bind(&c.doc_id)
            .bind(c.ordinal)
            .bind(&c.text)
            .bind(c.token_count)
            .bind(&c.metadata)
            .bind(vector_literal(emb))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn vector_search(&self, query: &[f32], k: usize) -> Result<Vec<Scored>> {
        // `<=>` is cosine distance in [0, 2]; similarity = 1 - distance.
        let rows = sqlx::query(
            "SELECT id, doc_id, ordinal, text, token_count, metadata,
                    (embedding <=> $1::vector) AS distance
             FROM chunks
             ORDER BY embedding <=> $1::vector
             LIMIT $2",
        )
        .bind(vector_literal(query))
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
                Ok(Document {
                    id: row.try_get("id")?,
                    source_uri: row.try_get("source_uri")?,
                    title: row.try_get("title")?,
                    hash: row.try_get("hash")?,
                    metadata: row.try_get("metadata")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    async fn clear(&self) -> Result<()> {
        sqlx::query("DELETE FROM chunks")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM documents")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
