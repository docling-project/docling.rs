//! Redis pub/sub message queue (feature `redis`).
//!
//! A subscription owns its pub/sub connection in a forwarding task that pushes
//! payloads into an mpsc channel, sidestepping the borrow between a `PubSub` and
//! its message stream. Compile-checked here; exercised against a live Redis.

use super::{MessageQueue, QueueReceiver, TOPIC};
use crate::{RagError, Result};
use async_trait::async_trait;
use futures_lite::StreamExt;
use redis::AsyncCommands;
use tokio::sync::mpsc;

/// Redis-backed pub/sub queue.
pub struct RedisQueue {
    client: redis::Client,
    conn: redis::aio::MultiplexedConnection,
}

impl RedisQueue {
    /// Connect to Redis at `url` (`redis://host:port`).
    pub async fn connect(url: &str) -> Result<Self> {
        let client =
            redis::Client::open(url).map_err(|e| RagError::Queue(format!("redis open: {e}")))?;
        let conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| RagError::Queue(format!("redis connect: {e}")))?;
        Ok(RedisQueue { client, conn })
    }
}

#[async_trait]
impl MessageQueue for RedisQueue {
    async fn publish(&self, payload: &[u8]) -> Result<()> {
        let mut conn = self.conn.clone();
        conn.publish::<_, _, ()>(TOPIC, payload)
            .await
            .map_err(|e| RagError::Queue(format!("redis publish: {e}")))?;
        Ok(())
    }

    async fn subscribe(&self) -> Result<Box<dyn QueueReceiver>> {
        let mut pubsub = self
            .client
            .get_async_pubsub()
            .await
            .map_err(|e| RagError::Queue(format!("redis pubsub: {e}")))?;
        pubsub
            .subscribe(TOPIC)
            .await
            .map_err(|e| RagError::Queue(format!("redis subscribe: {e}")))?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
        tokio::spawn(async move {
            let mut stream = pubsub.on_message();
            while let Some(msg) = stream.next().await {
                let payload: Vec<u8> = msg.get_payload().unwrap_or_default();
                if tx.send(payload).await.is_err() {
                    break;
                }
            }
        });
        Ok(Box::new(RedisReceiver { rx }))
    }
}

struct RedisReceiver {
    rx: mpsc::Receiver<Vec<u8>>,
}

#[async_trait]
impl QueueReceiver for RedisReceiver {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}
