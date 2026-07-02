//! Pluggable embedding providers.
//!
//! All providers implement [`Embedder`]. The default is [`ollama`] (`bge-m3`,
//! 1024-dim); [`gemini`] and the feature-gated `onnx` provider are alternatives,
//! and [`hash`] is a deterministic offline embedder for tests and evaluation.

mod hash;

#[cfg(feature = "onnx-embed")]
mod onnx;

pub mod gemini;
pub mod ollama;

pub use hash::HashEmbedder;

use crate::config::EmbedProvider;
use crate::{RagConfig, RagError, Result};
use async_trait::async_trait;
use std::sync::Arc;

/// A source of embedding vectors. Implementations must be cheap to `clone` via
/// `Arc` and safe to share across tasks.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts, returning one vector per input in order.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// The dimensionality of every returned vector.
    fn dim(&self) -> usize;

    /// A short identifier (`"ollama:bge-m3"`, `"hash"`, …) for logs and eval reports.
    fn id(&self) -> &str;

    /// Convenience: embed a single text.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(std::slice::from_ref(&text.to_string())).await?;
        v.pop()
            .ok_or_else(|| RagError::Embedding("provider returned no vector".into()))
    }
}

/// Build the embedder selected by `cfg.embed_provider`.
pub fn from_config(cfg: &RagConfig) -> Result<Arc<dyn Embedder>> {
    match cfg.embed_provider {
        EmbedProvider::Ollama => Ok(Arc::new(ollama::OllamaEmbedder::from_config(cfg))),
        EmbedProvider::Gemini => Ok(Arc::new(gemini::GeminiEmbedder::from_config(cfg)?)),
        EmbedProvider::Hash => Ok(Arc::new(HashEmbedder::new(cfg.embed_dim))),
        EmbedProvider::Onnx => {
            #[cfg(feature = "onnx-embed")]
            {
                Ok(Arc::new(onnx::OnnxEmbedder::from_config(cfg)?))
            }
            #[cfg(not(feature = "onnx-embed"))]
            {
                Err(RagError::FeatureDisabled(
                    "onnx".into(),
                    "onnx-embed".into(),
                ))
            }
        }
    }
}
