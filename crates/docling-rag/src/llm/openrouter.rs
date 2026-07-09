//! OpenRouter chat client (OpenAI-compatible `/chat/completions`). Model and key
//! are configurable; the default model is DeepSeek-V3 (`deepseek/deepseek-chat`).

use super::{ChatModel, Message};
use crate::{RagConfig, RagError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Chat model backed by OpenRouter.
#[derive(Debug, Clone)]
pub struct OpenRouterClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

#[derive(Serialize)]
struct ChatReq<'a> {
    model: &'a str,
    messages: &'a [Message],
    temperature: f32,
}

#[derive(Deserialize)]
struct ChatResp {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Deserialize)]
struct RespMessage {
    #[serde(default)]
    content: String,
}

impl OpenRouterClient {
    /// Build from config; errors if `OPENROUTER_API_KEY` is unset.
    pub fn from_config(cfg: &RagConfig) -> Result<Self> {
        let api_key = cfg.openrouter_api_key.clone().ok_or_else(|| {
            RagError::config("OPENROUTER_API_KEY is required for LLM-backed retrieval/synthesis")
        })?;
        Ok(OpenRouterClient {
            client: reqwest::Client::new(),
            base_url: cfg.openrouter_base_url.trim_end_matches('/').to_string(),
            api_key,
            model: cfg.llm_model.clone(),
        })
    }
}

#[async_trait]
impl ChatModel for OpenRouterClient {
    async fn complete(&self, messages: &[Message]) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        tracing::debug!(url = %url, model = %self.model, "llm chat request");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            // OpenRouter attribution headers (optional but recommended).
            .header(
                "HTTP-Referer",
                "https://github.com/docling-project/docling.rs",
            )
            .header("X-Title", "docling-rag")
            .json(&ChatReq {
                model: &self.model,
                messages,
                temperature: 0.2,
            })
            .send()
            .await?;
        // Surface the provider's error body — a bare 401 without it is
        // undiagnosable (wrong key kind, wrong base URL, unknown model, …).
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(400).collect();
            let hint = if status.as_u16() == 401 {
                " (hint: OpenRouter keys start with 'sk-or-'; for a native \
                 DeepSeek key set OPENROUTER_BASE_URL=https://api.deepseek.com \
                 and RAG_LLM_MODEL=deepseek-chat)"
            } else {
                ""
            };
            return Err(RagError::Llm(format!(
                "{url} returned {status}: {snippet}{hint}"
            )));
        }
        let body: ChatResp = resp.json().await?;
        body.choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| RagError::Llm("llm returned no choices".into()))
    }
}
