//! RabbitMQ (AMQP) message queue (feature `rabbitmq`, via `lapin`).
//!
//! Compile-checked here; exercised against a live broker.

use super::{MessageQueue, QueueReceiver, TOPIC};
use crate::{RagError, Result};
use async_trait::async_trait;
use futures_lite::StreamExt;
use lapin::options::{BasicConsumeOptions, BasicPublishOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};

/// AMQP-backed queue.
pub struct RabbitMqQueue {
    conn: Connection,
    channel: Channel,
}

impl RabbitMqQueue {
    /// Connect and declare the ingest queue.
    pub async fn connect(url: &str) -> Result<Self> {
        let conn = Connection::connect(url, ConnectionProperties::default())
            .await
            .map_err(|e| RagError::Queue(format!("amqp connect: {e}")))?;
        let channel = conn
            .create_channel()
            .await
            .map_err(|e| RagError::Queue(format!("amqp channel: {e}")))?;
        channel
            .queue_declare(TOPIC, QueueDeclareOptions::default(), FieldTable::default())
            .await
            .map_err(|e| RagError::Queue(format!("amqp queue_declare: {e}")))?;
        Ok(RabbitMqQueue { conn, channel })
    }
}

#[async_trait]
impl MessageQueue for RabbitMqQueue {
    async fn publish(&self, payload: &[u8]) -> Result<()> {
        let confirm = self
            .channel
            .basic_publish(
                "",
                TOPIC,
                BasicPublishOptions::default(),
                payload,
                BasicProperties::default(),
            )
            .await
            .map_err(|e| RagError::Queue(format!("amqp publish: {e}")))?;
        confirm
            .await
            .map_err(|e| RagError::Queue(format!("amqp confirm: {e}")))?;
        Ok(())
    }

    async fn subscribe(&self) -> Result<Box<dyn QueueReceiver>> {
        let channel = self
            .conn
            .create_channel()
            .await
            .map_err(|e| RagError::Queue(format!("amqp channel: {e}")))?;
        let consumer = channel
            .basic_consume(
                TOPIC,
                "docling-rag",
                BasicConsumeOptions {
                    no_ack: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| RagError::Queue(format!("amqp consume: {e}")))?;
        Ok(Box::new(RabbitReceiver { consumer }))
    }
}

struct RabbitReceiver {
    consumer: lapin::Consumer,
}

#[async_trait]
impl QueueReceiver for RabbitReceiver {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        match self.consumer.next().await {
            Some(Ok(delivery)) => Some(delivery.data),
            _ => None,
        }
    }
}
