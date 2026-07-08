//! Pluggable message queue for decoupling document discovery from ingestion.
//!
//! The default is an in-process [`memory`] channel; RabbitMQ and Redis pub/sub are
//! available behind the `rabbitmq` and `redis` features. Payloads are opaque byte
//! blobs (the pipeline publishes JSON-encoded [`crate::source::SourceRef`]s).

pub mod memory;

#[cfg(feature = "rabbitmq")]
pub mod rabbitmq;
#[cfg(feature = "redis")]
pub mod redis;

use crate::config::QueueKind;
use crate::{RagConfig, RagError, Result};
use async_trait::async_trait;
use std::sync::Arc;

/// The queue/topic name used across backends.
pub const TOPIC: &str = "docling_rag_ingest";

/// A subscription that yields published payloads until the queue closes.
#[async_trait]
pub trait QueueReceiver: Send {
    /// Await the next payload; `None` when the queue is closed / drained.
    async fn recv(&mut self) -> Option<Vec<u8>>;
}

/// A pluggable publish/subscribe message queue.
#[async_trait]
pub trait MessageQueue: Send + Sync {
    /// Publish one payload to all current subscribers.
    async fn publish(&self, payload: &[u8]) -> Result<()>;

    /// Open a new subscription.
    async fn subscribe(&self) -> Result<Box<dyn QueueReceiver>>;
}

/// Build the queue selected by `cfg.queue`.
pub async fn from_config(cfg: &RagConfig) -> Result<Arc<dyn MessageQueue>> {
    match cfg.queue {
        QueueKind::Memory => Ok(Arc::new(memory::MemoryQueue::new())),
        QueueKind::RabbitMq => {
            #[cfg(feature = "rabbitmq")]
            {
                let url = cfg.rabbitmq_url.clone().ok_or_else(|| {
                    RagError::config("RABBITMQ_URL is required for the rabbitmq queue")
                })?;
                Ok(Arc::new(rabbitmq::RabbitMqQueue::connect(&url).await?))
            }
            #[cfg(not(feature = "rabbitmq"))]
            {
                Err(RagError::FeatureDisabled(
                    "rabbitmq".into(),
                    "rabbitmq".into(),
                ))
            }
        }
        QueueKind::Redis => {
            #[cfg(feature = "redis")]
            {
                let url = cfg
                    .redis_url
                    .clone()
                    .ok_or_else(|| RagError::config("REDIS_URL is required for the redis queue"))?;
                Ok(Arc::new(redis::RedisQueue::connect(&url).await?))
            }
            #[cfg(not(feature = "redis"))]
            {
                Err(RagError::FeatureDisabled("redis".into(), "redis".into()))
            }
        }
    }
}
