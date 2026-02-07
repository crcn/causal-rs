//! Configuration for the Kafka backend

use serde::{Deserialize, Serialize};

/// Configuration for the Kafka backend.
///
/// This configures how Seesaw connects to and uses Kafka for event streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KafkaBackendConfig {
    /// Kafka broker addresses (e.g., ["localhost:9092", "broker2:9092"]).
    pub brokers: Vec<String>,

    /// Kafka topic for events (default: "seesaw.events").
    pub topic_events: String,

    /// Consumer group ID for event workers (default: "seesaw-event-workers").
    pub consumer_group: String,

    /// Number of partitions for the events topic.
    /// This determines the maximum parallelism for event processing.
    pub num_partitions: i32,

    /// Enable idempotent producer (recommended: true).
    /// This ensures exactly-once semantics at the Kafka producer level.
    pub enable_idempotent_producer: bool,

    /// Producer acks setting (default: "all").
    /// "all" means all in-sync replicas must acknowledge.
    pub producer_acks: String,

    /// Enable auto-commit for consumers (default: false).
    /// We manage commits manually for exactly-once semantics.
    pub enable_auto_commit: bool,

    /// Session timeout for consumers in milliseconds (default: 30000).
    pub session_timeout_ms: i32,

    /// Max poll interval for consumers in milliseconds (default: 300000).
    pub max_poll_interval_ms: i32,
}

impl Default for KafkaBackendConfig {
    fn default() -> Self {
        Self {
            brokers: vec!["localhost:9092".to_string()],
            topic_events: "seesaw.events".to_string(),
            consumer_group: "seesaw-event-workers".to_string(),
            num_partitions: 16,
            enable_idempotent_producer: true,
            producer_acks: "all".to_string(),
            enable_auto_commit: false,
            session_timeout_ms: 30000,
            max_poll_interval_ms: 300000,
        }
    }
}

impl KafkaBackendConfig {
    /// Create a new Kafka backend configuration.
    pub fn new(brokers: Vec<String>) -> Self {
        Self {
            brokers,
            ..Default::default()
        }
    }

    /// Set the events topic name.
    pub fn with_topic_events(mut self, topic: impl Into<String>) -> Self {
        self.topic_events = topic.into();
        self
    }

    /// Set the consumer group ID.
    pub fn with_consumer_group(mut self, group: impl Into<String>) -> Self {
        self.consumer_group = group.into();
        self
    }

    /// Set the number of partitions.
    pub fn with_num_partitions(mut self, partitions: i32) -> Self {
        self.num_partitions = partitions;
        self
    }

    /// Set whether to enable idempotent producer.
    pub fn with_idempotent_producer(mut self, enable: bool) -> Self {
        self.enable_idempotent_producer = enable;
        self
    }

    /// Get broker connection string (comma-separated).
    pub fn broker_list(&self) -> String {
        self.brokers.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = KafkaBackendConfig::default();
        assert_eq!(config.brokers, vec!["localhost:9092".to_string()]);
        assert_eq!(config.topic_events, "seesaw.events");
        assert_eq!(config.consumer_group, "seesaw-event-workers");
        assert_eq!(config.num_partitions, 16);
        assert!(config.enable_idempotent_producer);
        assert!(!config.enable_auto_commit);
    }

    #[test]
    fn test_builder_pattern() {
        let config = KafkaBackendConfig::new(vec!["broker1:9092".to_string()])
            .with_topic_events("custom.events")
            .with_consumer_group("custom-workers")
            .with_num_partitions(32)
            .with_idempotent_producer(false);

        assert_eq!(config.brokers, vec!["broker1:9092".to_string()]);
        assert_eq!(config.topic_events, "custom.events");
        assert_eq!(config.consumer_group, "custom-workers");
        assert_eq!(config.num_partitions, 32);
        assert!(!config.enable_idempotent_producer);
    }

    #[test]
    fn test_broker_list() {
        let config = KafkaBackendConfig::new(vec![
            "broker1:9092".to_string(),
            "broker2:9092".to_string(),
            "broker3:9092".to_string(),
        ]);

        assert_eq!(config.broker_list(), "broker1:9092,broker2:9092,broker3:9092");
    }
}
