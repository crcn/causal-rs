//! Kafka backend for Seesaw event-driven orchestration
//!
//! This crate provides a Kafka-based backend for Seesaw that combines:
//! - **Kafka** for event streaming (high throughput, horizontal scaling, replay)
//! - **PostgreSQL** for coordination state (idempotency, handler executions, joins, DLQ)
//!
//! # Architecture
//!
//! ```text
//! KafkaBackend
//! ├── Kafka (event streaming)
//! │   ├── seesaw.events topic (partitioned by correlation_id)
//! │   ├── Producer (publish events)
//! │   └── Consumer Groups (event workers)
//! └── PostgresStore (coordination state)
//!     ├── Idempotency ledger (seesaw_processed)
//!     ├── Handler executions (seesaw_handler_executions)
//!     ├── Join windows (seesaw_join_windows, seesaw_join_entries)
//!     └── DLQ (seesaw_dlq)
//! ```
//!
//! # Features
//!
//! - **High throughput**: 50k-100k events/sec (10-20x improvement over PostgreSQL-only)
//! - **Horizontal scaling**: Scale via Kafka partitions and consumer groups
//! - **Event replay**: Replay events from Kafka topic for debugging/analysis
//! - **Exactly-once semantics**: Kafka offset commits + PostgreSQL idempotency
//! - **Cross-platform integration**: Other systems can consume from Kafka
//! - **Same API**: Drop-in replacement for PostgresBackend
//!
//! # Example
//!
//! ```rust,no_run
//! use seesaw_kafka::{KafkaBackend, KafkaBackendConfig};
//! use seesaw_postgres::PostgresStore;
//! use sqlx::postgres::PgPoolOptions;
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Create PostgreSQL pool
//! let pool = PgPoolOptions::new()
//!     .max_connections(20)
//!     .connect("postgres://localhost/seesaw")
//!     .await?;
//!
//! let store = PostgresStore::new(pool);
//!
//! // Configure Kafka
//! let kafka_config = KafkaBackendConfig::new(vec!["localhost:9092".to_string()])
//!     .with_topic_events("seesaw.events")
//!     .with_consumer_group("seesaw-workers")
//!     .with_num_partitions(16);
//!
//! // Create Kafka backend
//! let backend = KafkaBackend::new(kafka_config, store)?;
//!
//! // Use with Engine (same API as PostgresBackend)
//! // let engine = Engine::new(deps, backend)
//! //     .with_handler(handler::on::<OrderPlaced>().then(...))
//! //     .start()
//! //     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Guarantees
//!
//! - **Per-workflow FIFO ordering**: Events for same `correlation_id` always go to same partition
//! - **Exactly-once processing**: Idempotency checks prevent duplicate processing on redelivery
//! - **Crash recovery**: Kafka redelivers uncommitted messages; idempotency prevents duplicates
//! - **Atomic commits**: PostgreSQL transaction includes all side effects
//!
//! # Performance
//!
//! - **PostgreSQL backend**: ~1-5k events/sec (single-node bottleneck)
//! - **Kafka backend**: ~50-100k events/sec (horizontal scaling via partitions)
//! - **Throughput improvement**: 10-20x
//!
//! # Trade-offs
//!
//! **Pros:**
//! - ✅ 10-20x throughput improvement
//! - ✅ Horizontal scaling via Kafka partitions
//! - ✅ Event replay capabilities
//! - ✅ Cross-platform integration
//! - ✅ No breaking changes to Seesaw API
//!
//! **Cons:**
//! - ❌ Operational complexity (requires Kafka cluster)
//! - ❌ Two systems to manage (Kafka + PostgreSQL)
//! - ❌ Join operations still require PostgreSQL
//! - ❌ Not pure Kafka (hybrid architecture)

pub mod backend;
pub mod config;
pub mod event_worker;
pub mod kafka_client;
pub mod partition_strategy;

pub use backend::KafkaBackend;
pub use config::KafkaBackendConfig;
pub use kafka_client::{KafkaClient, KafkaEventMessage};
pub use partition_strategy::compute_partition;
