//! Kafka client wrapper for producing and consuming events

use anyhow::{Context, Result};
use rdkafka::client::ClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{CommitMode, Consumer, ConsumerContext, StreamConsumer};
use rdkafka::error::KafkaError;
use rdkafka::message::OwnedMessage;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::Message;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::config::KafkaBackendConfig;

/// Serialized event message for Kafka.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KafkaEventMessage {
    pub event_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub hops: i32,
    pub retry_count: i32,
    pub batch_id: Option<Uuid>,
    pub batch_index: Option<i32>,
    pub batch_size: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Custom context for Kafka consumer with logging.
pub struct LoggingConsumerContext;

impl ClientContext for LoggingConsumerContext {}

impl ConsumerContext for LoggingConsumerContext {
    fn commit_callback(&self, result: Result<(), KafkaError>, _offsets: &rdkafka::TopicPartitionList) {
        match result {
            Ok(_) => debug!("Successfully committed offsets"),
            Err(e) => warn!("Error committing offsets: {:?}", e),
        }
    }
}

/// Kafka client for producing and consuming events.
pub struct KafkaClient {
    producer: FutureProducer,
    config: Arc<KafkaBackendConfig>,
}

impl KafkaClient {
    /// Create a new Kafka client.
    pub fn new(config: KafkaBackendConfig) -> Result<Self> {
        let mut client_config = ClientConfig::new();
        client_config.set("bootstrap.servers", &config.broker_list());

        if config.enable_idempotent_producer {
            client_config.set("enable.idempotence", "true");
        }

        client_config.set("acks", &config.producer_acks);
        client_config.set("compression.type", "snappy");
        client_config.set("linger.ms", "5");
        client_config.set("batch.size", "32768");

        let producer = client_config
            .create()
            .context("Failed to create Kafka producer")?;

        Ok(Self {
            producer,
            config: Arc::new(config),
        })
    }

    /// Produce an event to Kafka.
    ///
    /// # Arguments
    /// * `partition` - The target partition
    /// * `message` - The event message to produce
    ///
    /// # Returns
    /// Ok(()) on success, or an error if the produce fails
    pub async fn produce(&self, partition: i32, message: &KafkaEventMessage) -> Result<()> {
        let payload = serde_json::to_vec(message)
            .context("Failed to serialize event message")?;

        let key = message.correlation_id.as_bytes();

        let record = FutureRecord::to(&self.config.topic_events)
            .partition(partition)
            .key(key)
            .payload(&payload);

        self.producer
            .send(record, Duration::from_secs(5))
            .await
            .map_err(|(err, _)| err)
            .context("Failed to produce message to Kafka")?;

        debug!(
            "Produced event {} to partition {} of topic {}",
            message.event_id, partition, self.config.topic_events
        );

        Ok(())
    }

    /// Create a consumer for the events topic.
    ///
    /// # Arguments
    /// * `worker_id` - Unique identifier for this consumer (for debugging)
    ///
    /// # Returns
    /// A configured StreamConsumer
    pub fn create_consumer(&self, worker_id: usize) -> Result<StreamConsumer<LoggingConsumerContext>> {
        let mut client_config = ClientConfig::new();
        client_config.set("bootstrap.servers", &self.config.broker_list());
        client_config.set("group.id", &self.config.consumer_group);
        client_config.set("enable.auto.commit", &self.config.enable_auto_commit.to_string());
        client_config.set("session.timeout.ms", &self.config.session_timeout_ms.to_string());
        client_config.set("max.poll.interval.ms", &self.config.max_poll_interval_ms.to_string());
        client_config.set("enable.partition.eof", "false");
        client_config.set("auto.offset.reset", "earliest");

        // Set client ID for debugging
        client_config.set("client.id", &format!("seesaw-event-worker-{}", worker_id));

        let consumer: StreamConsumer<LoggingConsumerContext> = client_config
            .create_with_context(LoggingConsumerContext)
            .context("Failed to create Kafka consumer")?;

        consumer
            .subscribe(&[&self.config.topic_events])
            .context("Failed to subscribe to events topic")?;

        debug!(
            "Created consumer {} for topic {} in group {}",
            worker_id, self.config.topic_events, self.config.consumer_group
        );

        Ok(consumer)
    }
}

/// Helper to deserialize a Kafka message into an event.
pub fn deserialize_event_message(msg: &OwnedMessage) -> Result<KafkaEventMessage> {
    let payload = msg
        .payload()
        .context("Message has no payload")?;

    serde_json::from_slice(payload)
        .context("Failed to deserialize event message")
}

/// Helper to commit a message offset.
pub async fn commit_message_offset(consumer: &StreamConsumer<LoggingConsumerContext>, msg: &OwnedMessage) -> Result<()> {
    // Build topic partition list for manual offset commit
    let mut tpl = rdkafka::TopicPartitionList::new();
    tpl.add_partition_offset(
        msg.topic(),
        msg.partition(),
        rdkafka::Offset::Offset(msg.offset() + 1),
    )?;

    consumer
        .commit(&tpl, CommitMode::Sync)
        .context("Failed to commit message offset")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_message_serialization() {
        let msg = KafkaEventMessage {
            event_id: Uuid::new_v4(),
            parent_id: Some(Uuid::new_v4()),
            correlation_id: Uuid::new_v4(),
            event_type: "TestEvent".to_string(),
            payload: serde_json::json!({"test": "data"}),
            hops: 1,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: chrono::Utc::now(),
        };

        let serialized = serde_json::to_vec(&msg).unwrap();
        let deserialized: KafkaEventMessage = serde_json::from_slice(&serialized).unwrap();

        assert_eq!(msg.event_id, deserialized.event_id);
        assert_eq!(msg.event_type, deserialized.event_type);
        assert_eq!(msg.correlation_id, deserialized.correlation_id);
    }
}
