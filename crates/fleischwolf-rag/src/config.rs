//! Configuration, loaded from the process environment (and a `.env` file).
//!
//! Every knob has a documented default so the crate runs out of the box with the
//! offline-friendly stack (bundled SQLite + Ollama). See `.env.example` at the
//! repo root for the full list.

use crate::model::RetrievalMode;
use crate::{RagError, Result};
use std::str::FromStr;

/// Which database backend backs the [`crate::store::VectorStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbBackend {
    /// Bundled SQLite (default, zero external services).
    Sqlite,
    /// PostgreSQL (+ pgvector). Requires the `postgres` cargo feature.
    Postgres,
    /// Pure in-memory store — never persisted; used by tests and quick evals.
    Memory,
}

/// Which embedding provider produces vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedProvider {
    /// Ollama HTTP API (default; e.g. `bge-m3`, 1024-dim).
    Ollama,
    /// Google Gemini embeddings (`gemini-embedding-001`, truncated to `dim`).
    Gemini,
    /// Local ONNX model. Requires the `onnx-embed` cargo feature.
    Onnx,
    /// Deterministic hashing embedder — no network, used for offline tests/eval.
    Hash,
}

/// Which document source feeds the ingestion pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A local directory (works over any FUSE / network mount).
    Folder,
    /// FTP. Requires the `remote-sources` cargo feature.
    Ftp,
    /// SFTP. Requires the `remote-sources` cargo feature.
    Sftp,
}

/// Which message queue drives async ingestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    /// In-process tokio channel (default).
    Memory,
    /// RabbitMQ (AMQP). Requires the `rabbitmq` cargo feature.
    RabbitMq,
    /// Redis pub/sub. Requires the `redis` cargo feature.
    Redis,
}

/// The fully-resolved configuration for a RAG session.
#[derive(Debug, Clone)]
pub struct RagConfig {
    // --- database ---
    pub db_backend: DbBackend,
    pub database_url: String,

    // --- embedding ---
    pub embed_provider: EmbedProvider,
    pub embed_model: String,
    pub embed_dim: usize,
    pub ollama_base_url: String,
    pub gemini_api_key: Option<String>,
    pub gemini_model: String,
    pub embed_onnx_path: String,
    pub embed_tokenizer_path: String,

    // --- llm (OpenRouter) ---
    pub openrouter_api_key: Option<String>,
    pub openrouter_base_url: String,
    pub llm_model: String,

    // --- chunking ---
    pub chunk_size: usize,
    pub chunk_overlap: f32,
    pub chunk_unit: ChunkUnit,

    // --- retrieval ---
    pub retrieval_mode: RetrievalMode,
    pub top_k: usize,
    pub rrf_k: f32,
    pub multiquery_n: usize,

    // --- sources ---
    pub source: SourceKind,
    pub source_path: String,
    pub source_url: Option<String>,
    pub source_user: Option<String>,
    pub source_password: Option<String>,
    /// Optional folder to dump each ingested document's converted Markdown into
    /// (for debugging / re-ingestion). `None` disables the dump.
    pub documents_output: Option<String>,

    // --- queue ---
    pub queue: QueueKind,
    pub rabbitmq_url: Option<String>,
    pub redis_url: Option<String>,

    // --- REST API ---
    /// Bind address for `serve` (default `127.0.0.1:8080`).
    pub http_addr: String,
    /// Accepted API keys (comma-separated in `RAG_API_KEYS`). The server refuses
    /// to start with an empty list — auth is fail-closed.
    pub api_keys: Vec<String>,
}

/// The unit used to measure chunk size / overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkUnit {
    /// Whitespace-delimited words (fast, no tokenizer). Default.
    Word,
    /// Approximate subword tokens (`chars/4` heuristic — no tokenizer dependency).
    Token,
}

impl Default for RagConfig {
    fn default() -> Self {
        RagConfig {
            db_backend: DbBackend::Sqlite,
            database_url: "sqlite://data/rag.db".to_string(),
            embed_provider: EmbedProvider::Ollama,
            embed_model: "bge-m3".to_string(),
            embed_dim: 1024,
            ollama_base_url: "http://localhost:11434".to_string(),
            gemini_api_key: None,
            gemini_model: "gemini-embedding-001".to_string(),
            embed_onnx_path: "models/embed/bge-m3.onnx".to_string(),
            embed_tokenizer_path: "models/embed/tokenizer.json".to_string(),
            openrouter_api_key: None,
            openrouter_base_url: "https://openrouter.ai/api/v1".to_string(),
            llm_model: "deepseek/deepseek-chat".to_string(),
            chunk_size: 300,
            chunk_overlap: 0.05,
            chunk_unit: ChunkUnit::Word,
            retrieval_mode: RetrievalMode::Hybrid,
            top_k: 5,
            rrf_k: 60.0,
            multiquery_n: 4,
            source: SourceKind::Folder,
            source_path: "./input".to_string(),
            source_url: None,
            source_user: None,
            source_password: None,
            documents_output: None,
            queue: QueueKind::Memory,
            rabbitmq_url: None,
            redis_url: None,
            http_addr: "127.0.0.1:8080".to_string(),
            api_keys: Vec::new(),
        }
    }
}

impl RagConfig {
    /// Load a `.env` file (if present) then resolve config from the environment.
    ///
    /// Missing keys fall back to the [`Default`] values, so this never fails on an
    /// empty environment; it only errors on an *invalid* value (bad number, unknown
    /// backend name).
    pub fn from_env() -> Result<Self> {
        // Best-effort: a missing .env is not an error.
        let _ = dotenvy::dotenv();
        Self::from_env_inner()
    }

    fn from_env_inner() -> Result<Self> {
        let d = RagConfig::default();
        let cfg = RagConfig {
            db_backend: match env_str("RAG_DB_BACKEND") {
                Some(s) => parse_db_backend(&s)?,
                None => d.db_backend,
            },
            database_url: env_str("RAG_DATABASE_URL").unwrap_or(d.database_url),
            embed_provider: match env_str("RAG_EMBED_PROVIDER") {
                Some(s) => parse_embed_provider(&s)?,
                None => d.embed_provider,
            },
            embed_model: env_str("RAG_EMBED_MODEL").unwrap_or(d.embed_model),
            embed_dim: env_parse("RAG_EMBED_DIM", d.embed_dim)?,
            ollama_base_url: env_str("OLLAMA_BASE_URL").unwrap_or(d.ollama_base_url),
            gemini_api_key: env_str("GEMINI_API_KEY"),
            gemini_model: env_str("RAG_GEMINI_MODEL").unwrap_or(d.gemini_model),
            embed_onnx_path: env_str("RAG_EMBED_ONNX_PATH").unwrap_or(d.embed_onnx_path),
            embed_tokenizer_path: env_str("RAG_EMBED_TOKENIZER").unwrap_or(d.embed_tokenizer_path),
            openrouter_api_key: env_str("OPENROUTER_API_KEY"),
            openrouter_base_url: env_str("OPENROUTER_BASE_URL").unwrap_or(d.openrouter_base_url),
            llm_model: env_str("RAG_LLM_MODEL").unwrap_or(d.llm_model),
            chunk_size: env_parse("RAG_CHUNK_SIZE", d.chunk_size)?,
            chunk_overlap: env_parse("RAG_CHUNK_OVERLAP", d.chunk_overlap)?,
            chunk_unit: match env_str("RAG_CHUNK_UNIT") {
                Some(s) => parse_chunk_unit(&s)?,
                None => d.chunk_unit,
            },
            retrieval_mode: match env_str("RAG_RETRIEVAL_MODE") {
                Some(s) => RetrievalMode::from_str(&s)?,
                None => d.retrieval_mode,
            },
            top_k: env_parse("RAG_TOP_K", d.top_k)?,
            rrf_k: env_parse("RAG_RRF_K", d.rrf_k)?,
            multiquery_n: env_parse("RAG_MULTIQUERY_N", d.multiquery_n)?,
            source: match env_str("RAG_SOURCE") {
                Some(s) => parse_source_kind(&s)?,
                None => d.source,
            },
            source_path: env_str("RAG_SOURCE_PATH").unwrap_or(d.source_path),
            source_url: env_str("RAG_SOURCE_URL"),
            source_user: env_str("RAG_SOURCE_USER"),
            source_password: env_str("RAG_SOURCE_PASSWORD"),
            documents_output: env_str("RAG_DOCUMENTS_OUTPUT"),
            queue: match env_str("RAG_QUEUE") {
                Some(s) => parse_queue_kind(&s)?,
                None => d.queue,
            },
            rabbitmq_url: env_str("RABBITMQ_URL"),
            redis_url: env_str("REDIS_URL"),
            http_addr: env_str("RAG_HTTP_ADDR").unwrap_or(d.http_addr),
            api_keys: env_str("RAG_API_KEYS")
                .map(|s| {
                    s.split(',')
                        .map(|k| k.trim().to_string())
                        .filter(|k| !k.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Sanity-check numeric ranges. Returns the first violation.
    pub fn validate(&self) -> Result<()> {
        if self.embed_dim == 0 {
            return Err(RagError::config("RAG_EMBED_DIM must be > 0"));
        }
        if self.chunk_size == 0 {
            return Err(RagError::config("RAG_CHUNK_SIZE must be > 0"));
        }
        if !(0.0..0.95).contains(&self.chunk_overlap) {
            return Err(RagError::config("RAG_CHUNK_OVERLAP must be in [0.0, 0.95)"));
        }
        if self.top_k == 0 {
            return Err(RagError::config("RAG_TOP_K must be > 0"));
        }
        Ok(())
    }
}

fn env_str(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn env_parse<T>(key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match env_str(key) {
        Some(s) => s
            .parse::<T>()
            .map_err(|e| RagError::config(format!("{key}: invalid value '{s}': {e}"))),
        None => Ok(default),
    }
}

fn parse_db_backend(s: &str) -> Result<DbBackend> {
    match s.to_ascii_lowercase().as_str() {
        "sqlite" => Ok(DbBackend::Sqlite),
        "postgres" | "postgresql" | "pg" => Ok(DbBackend::Postgres),
        "memory" | "mem" | "inmemory" => Ok(DbBackend::Memory),
        other => Err(RagError::config(format!(
            "unknown RAG_DB_BACKEND '{other}'"
        ))),
    }
}

fn parse_embed_provider(s: &str) -> Result<EmbedProvider> {
    match s.to_ascii_lowercase().as_str() {
        "ollama" => Ok(EmbedProvider::Ollama),
        "gemini" | "google" => Ok(EmbedProvider::Gemini),
        "onnx" | "local" => Ok(EmbedProvider::Onnx),
        "hash" | "test" | "fake" => Ok(EmbedProvider::Hash),
        other => Err(RagError::config(format!(
            "unknown RAG_EMBED_PROVIDER '{other}'"
        ))),
    }
}

fn parse_chunk_unit(s: &str) -> Result<ChunkUnit> {
    match s.to_ascii_lowercase().as_str() {
        "word" | "words" => Ok(ChunkUnit::Word),
        "token" | "tokens" => Ok(ChunkUnit::Token),
        other => Err(RagError::config(format!(
            "unknown RAG_CHUNK_UNIT '{other}'"
        ))),
    }
}

fn parse_source_kind(s: &str) -> Result<SourceKind> {
    match s.to_ascii_lowercase().as_str() {
        "folder" | "dir" | "directory" | "local" => Ok(SourceKind::Folder),
        "ftp" => Ok(SourceKind::Ftp),
        "sftp" => Ok(SourceKind::Sftp),
        other => Err(RagError::config(format!("unknown RAG_SOURCE '{other}'"))),
    }
}

fn parse_queue_kind(s: &str) -> Result<QueueKind> {
    match s.to_ascii_lowercase().as_str() {
        "memory" | "mem" | "inproc" => Ok(QueueKind::Memory),
        "rabbitmq" | "amqp" | "rabbit" => Ok(QueueKind::RabbitMq),
        "redis" => Ok(QueueKind::Redis),
        other => Err(RagError::config(format!("unknown RAG_QUEUE '{other}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_match_spec() {
        let c = RagConfig::default();
        c.validate().unwrap();
        assert_eq!(c.chunk_size, 300);
        assert!((c.chunk_overlap - 0.05).abs() < 1e-6);
        assert_eq!(c.embed_dim, 1024);
        assert_eq!(c.embed_model, "bge-m3");
        assert_eq!(c.llm_model, "deepseek/deepseek-chat");
        assert_eq!(c.retrieval_mode, RetrievalMode::Hybrid);
    }

    #[test]
    fn validate_rejects_bad_overlap() {
        let c = RagConfig {
            chunk_overlap: 0.99,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = RagConfig {
            chunk_size: 0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn backend_parsers() {
        assert_eq!(parse_db_backend("Postgres").unwrap(), DbBackend::Postgres);
        assert_eq!(parse_embed_provider("HASH").unwrap(), EmbedProvider::Hash);
        assert_eq!(parse_queue_kind("amqp").unwrap(), QueueKind::RabbitMq);
        assert!(parse_source_kind("carrier-pigeon").is_err());
    }
}
