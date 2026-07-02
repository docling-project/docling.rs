//! Google Gemini embedding provider. Uses the Generative Language API's
//! `embedContent` endpoint with `outputDimensionality` so `gemini-embedding-001`
//! (natively 3072-dim, Matryoshka-truncatable) returns the configured dimension.

use super::Embedder;
use crate::{RagConfig, RagError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Embedder backed by the Gemini API.
#[derive(Debug, Clone)]
pub struct GeminiEmbedder {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dim: usize,
    id: String,
}

#[derive(Serialize)]
struct EmbedContentReq<'a> {
    model: String,
    content: Content<'a>,
    #[serde(rename = "outputDimensionality")]
    output_dim: usize,
}

#[derive(Serialize)]
struct Content<'a> {
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct Part<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct EmbedContentResp {
    embedding: Embedding,
}

#[derive(Deserialize)]
struct Embedding {
    #[serde(default)]
    values: Vec<f32>,
}

impl GeminiEmbedder {
    /// Build from config; errors if `GEMINI_API_KEY` is unset.
    pub fn from_config(cfg: &RagConfig) -> Result<Self> {
        let api_key = cfg.gemini_api_key.clone().ok_or_else(|| {
            RagError::config("GEMINI_API_KEY is required for the gemini provider")
        })?;
        Ok(GeminiEmbedder {
            client: reqwest::Client::new(),
            api_key,
            model: cfg.gemini_model.clone(),
            dim: cfg.embed_dim,
            id: format!("gemini:{}", cfg.gemini_model),
        })
    }
}

#[async_trait]
impl Embedder for GeminiEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // The public API embeds one content per call; issue them sequentially to
        // stay well within rate limits (batch endpoints exist but add complexity).
        let url = format!("{API_BASE}/models/{}:embedContent", self.model);
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let req = EmbedContentReq {
                model: format!("models/{}", self.model),
                content: Content {
                    parts: vec![Part { text }],
                },
                output_dim: self.dim,
            };
            let resp = self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .json(&req)
                .send()
                .await?
                .error_for_status()?;
            let body: EmbedContentResp = resp.json().await?;
            if body.embedding.values.is_empty() {
                return Err(RagError::Embedding(
                    "gemini returned an empty embedding".into(),
                ));
            }
            out.push(body.embedding.values);
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        &self.id
    }
}
