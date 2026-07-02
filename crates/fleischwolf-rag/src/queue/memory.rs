//! In-process message queue built on a tokio broadcast channel.

use super::{MessageQueue, QueueReceiver};
use crate::Result;
use async_trait::async_trait;
use tokio::sync::broadcast;

/// A fan-out, in-process queue. Subscribers created before a publish receive it.
pub struct MemoryQueue {
    tx: broadcast::Sender<Vec<u8>>,
}

impl MemoryQueue {
    /// Create a queue with a default buffer capacity.
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    /// Create a queue with a specific per-subscriber buffer capacity.
    pub fn with_capacity(cap: usize) -> Self {
        let (tx, _rx) = broadcast::channel(cap);
        MemoryQueue { tx }
    }
}

impl Default for MemoryQueue {
    fn default() -> Self {
        MemoryQueue::new()
    }
}

#[async_trait]
impl MessageQueue for MemoryQueue {
    async fn publish(&self, payload: &[u8]) -> Result<()> {
        // Err only means "no subscribers"; that is not a failure to publish.
        let _ = self.tx.send(payload.to_vec());
        Ok(())
    }

    async fn subscribe(&self) -> Result<Box<dyn QueueReceiver>> {
        Ok(Box::new(MemoryReceiver {
            rx: self.tx.subscribe(),
        }))
    }
}

struct MemoryReceiver {
    rx: broadcast::Receiver<Vec<u8>>,
}

#[async_trait]
impl QueueReceiver for MemoryReceiver {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.rx.recv().await {
                Ok(v) => return Some(v),
                // A slow consumer that lagged: skip dropped messages, keep going.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_to_subscriber() {
        let q = MemoryQueue::new();
        let mut sub = q.subscribe().await.unwrap();
        q.publish(b"hello").await.unwrap();
        assert_eq!(sub.recv().await, Some(b"hello".to_vec()));
    }
}
