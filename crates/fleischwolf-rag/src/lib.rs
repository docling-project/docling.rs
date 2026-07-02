//! `fleischwolf-rag`: a pluggable Retrieval-Augmented-Generation subsystem built on
//! the [`fleischwolf`](https://crates.io/crates/fleischwolf) document converter.
//!
//! The pipeline is: **source → convert to Markdown → chunk → embed → vector store →
//! retrieve → (optionally) synthesize an answer**. Every external dependency is a
//! trait with swappable backends:
//!
//! - **Embedders** ([`embed`]): Ollama (default), Gemini, local ONNX, or a
//!   deterministic hashing embedder for offline tests.
//! - **Vector stores** ([`store`]): SQLite (default), PostgreSQL + pgvector, or
//!   in-memory. Documents and chunks live in separate tables.
//! - **Retrieval** ([`retrieve`]): dense vector, sparse BM25, Hybrid (RRF),
//!   Multi-Query fusion, and HyDE.
//! - **LLM** ([`llm`]): OpenRouter (default model DeepSeek-V3).
//! - **Sources** ([`source`]): local folder (default), FTP, SFTP.
//! - **Queues** ([`queue`]): in-process, RabbitMQ, Redis pub/sub.
//!
//! Configuration comes from the environment / a `.env` file via [`RagConfig`].
//!
//! ```no_run
//! use fleischwolf_rag::{RagConfig, Pipeline, RetrievalMode};
//!
//! # async fn run() -> fleischwolf_rag::Result<()> {
//! let cfg = RagConfig::from_env()?;
//! let pipeline = Pipeline::from_config(&cfg).await?;
//! pipeline.ingest_all().await?;
//! let hits = pipeline.query(RetrievalMode::Hybrid, "how does chunking work?", 5).await?;
//! for h in hits {
//!     println!("{:.3}  {}", h.score, h.chunk.text);
//! }
//! # Ok(()) }
//! ```

pub mod api;
pub mod chunk;
pub mod config;
pub mod embed;
pub mod error;
pub mod eval;
pub mod llm;
pub mod math;
pub mod metrics;
pub mod model;
pub mod pipeline;
pub mod queue;
pub mod retrieve;
pub mod source;
pub mod store;

pub use config::RagConfig;
pub use error::{RagError, Result};
pub use model::{Chunk, Document, RetrievalMode, Scored};
pub use pipeline::{Answer, IngestOutcome, IngestReport, Pipeline};
pub use retrieve::Retriever;
