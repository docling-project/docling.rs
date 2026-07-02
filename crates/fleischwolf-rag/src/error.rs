//! Error type for the RAG subsystem.

use std::fmt;

/// The crate-wide result alias.
pub type Result<T> = std::result::Result<T, RagError>;

/// Every fallible operation in the crate surfaces one of these.
#[derive(Debug, thiserror::Error)]
pub enum RagError {
    /// Configuration was missing or invalid (bad env var, unknown backend name, …).
    #[error("configuration error: {0}")]
    Config(String),

    /// A document could not be converted to Markdown by `fleischwolf`.
    #[error("document conversion failed: {0}")]
    Conversion(String),

    /// An embedding provider (Ollama / Gemini / ONNX / …) failed.
    #[error("embedding error: {0}")]
    Embedding(String),

    /// The vector store / database failed.
    #[error("store error: {0}")]
    Store(String),

    /// The chat/LLM provider (OpenRouter) failed.
    #[error("llm error: {0}")]
    Llm(String),

    /// A document source (folder / FTP / SFTP) failed.
    #[error("source error: {0}")]
    Source(String),

    /// A message queue backend failed.
    #[error("queue error: {0}")]
    Queue(String),

    /// An HTTP request failed.
    #[error("http error: {0}")]
    Http(String),

    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A feature-gated backend was requested but the crate was built without it.
    #[error("backend '{0}' is not available: rebuild with the '{1}' cargo feature")]
    FeatureDisabled(String, String),
}

impl RagError {
    /// Helper to build a [`RagError::Config`] from anything printable.
    pub fn config(msg: impl fmt::Display) -> Self {
        RagError::Config(msg.to_string())
    }
}

impl From<reqwest::Error> for RagError {
    fn from(e: reqwest::Error) -> Self {
        RagError::Http(e.to_string())
    }
}

impl From<sqlx::Error> for RagError {
    fn from(e: sqlx::Error) -> Self {
        RagError::Store(e.to_string())
    }
}
