//! Pluggable chat/LLM provider, used for Multi-Query and HyDE retrieval and for
//! final answer synthesis. The only shipped backend is [`openrouter`].

pub mod openrouter;

use crate::{RagConfig, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A single chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// `"system"`, `"user"`, or `"assistant"`.
    pub role: String,
    /// The message text.
    pub content: String,
}

impl Message {
    /// A system message.
    pub fn system(content: impl Into<String>) -> Self {
        Message {
            role: "system".into(),
            content: content.into(),
        }
    }
    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Message {
            role: "user".into(),
            content: content.into(),
        }
    }
}

/// A chat completion model.
#[async_trait]
pub trait ChatModel: Send + Sync {
    /// Complete a chat conversation, returning the assistant's reply text.
    async fn complete(&self, messages: &[Message]) -> Result<String>;

    /// Convenience: single system + user turn.
    async fn ask(&self, system: &str, user: &str) -> Result<String> {
        self.complete(&[Message::system(system), Message::user(user)])
            .await
    }
}

/// Build the configured chat model. Errors if the provider needs credentials that
/// are not set (`OPENROUTER_API_KEY`).
pub fn from_config(cfg: &RagConfig) -> Result<Arc<dyn ChatModel>> {
    Ok(Arc::new(openrouter::OpenRouterClient::from_config(cfg)?))
}
