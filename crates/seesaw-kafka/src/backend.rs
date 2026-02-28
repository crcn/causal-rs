//! Kafka backend implementation for Seesaw
//!
//! This backend uses:
//! - Kafka for event streaming (high throughput, horizontal scaling, replay)
//! - PostgreSQL for coordination state (idempotency, handler executions, joins, DLQ)

use anyhow::{Context, Result};
use async_trait::async_trait;
use seesaw_core::backend::job_executor::{HandlerStatus, JobExecutor};
use seesaw_core::backend::{Backend, BackendServeConfig, DispatchedEvent};
use seesaw_core::{DirectRunner, Store};

static DIRECT_RUNNER: DirectRunner = DirectRunner;
use seesaw_postgres::PostgresStore;
use std::sync::Arc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::config::KafkaBackendConfig;
use crate::event_worker::spawn_kafka_event_worker;
use crate::kafka_client::{KafkaClient, KafkaEventMessage};
use crate::partition_strategy::compute_partition;

/// Kafka backend for Seesaw.
///
/// This backend combines Kafka (for event streaming) with PostgreSQL (for coordination).
/// It provides:
/// - High throughput event processing (50k-100k events/sec)
/// - Horizontal scaling via Kafka partitions and consumer groups
/// - Event replay and time-travel capabilities
/// - Exactly-once semantics via Kafka offset commits + PostgreSQL idempotency
pub struct KafkaBackend {
    kafka: Arc<KafkaClient>,
    store: PostgresStore,
    config: Arc<KafkaBackendConfig>,
}

impl KafkaBackend {
    /// Create a new Kafka backend.
    ///
    /// # Arguments
    /// * `kafka_config` - Kafka configuration (brokers, topics, partitions)
    /// * `store` - PostgreSQL store for coordination state
    pub fn new(kafka_config: KafkaBackendConfig, store: PostgresStore) -> Result<Self> {
        let kafka = KafkaClient::new(kafka_config.clone())
            .context("Failed to create Kafka client")?;

        Ok(Self {
            kafka: Arc::new(kafka),
            store,
            config: Arc::new(kafka_config),
        })
    }

    /// Get reference to the Kafka client.
    pub fn kafka(&self) -> &KafkaClient {
        &self.kafka
    }

    /// Get reference to the PostgreSQL store.
    pub fn store(&self) -> &PostgresStore {
        &self.store
    }
}

impl Clone for KafkaBackend {
    fn clone(&self) -> Self {
        Self {
            kafka: self.kafka.clone(),
            store: self.store.clone(),
            config: self.config.clone(),
        }
    }
}

#[async_trait]
impl Backend for KafkaBackend {
    fn name(&self) -> &'static str {
        "kafka"
    }

    async fn publish(&self, event: DispatchedEvent) -> Result<()> {
        // Check idempotency first (avoid duplicate Kafka messages)
        if self.store.is_processed(event.event_id).await? {
            info!("Event {} already processed, skipping publish", event.event_id);
            return Ok(());
        }

        // Compute partition by correlation_id (ensures per-workflow FIFO)
        let partition = compute_partition(&event.correlation_id, self.config.num_partitions);

        // Convert to Kafka message
        let message = KafkaEventMessage {
            event_id: event.event_id,
            parent_id: event.parent_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type,
            payload: event.payload,
            hops: event.hops,
            retry_count: event.retry_count,
            batch_id: event.batch_id,
            batch_index: event.batch_index,
            batch_size: event.batch_size,
            created_at: event.created_at,
        };

        // Produce to Kafka
        self.kafka.produce(partition, &message).await?;

        info!(
            "Published event {} to Kafka partition {} (correlation_id: {})",
            event.event_id, partition, event.correlation_id
        );

        Ok(())
    }

    async fn serve<D>(
        &self,
        executor: Arc<JobExecutor<D>>,
        config: BackendServeConfig,
        shutdown: CancellationToken,
    ) -> Result<()>
    where
        D: Send + Sync + 'static,
    {
        let mut handles = Vec::new();

        // Spawn Kafka event workers
        for i in 0..config.event_workers {
            let consumer = self
                .kafka
                .create_consumer(i)
                .context("Failed to create Kafka consumer")?;
            let executor = executor.clone();
            let store = self.store.clone();
            let event_config = config.event_worker.clone();
            let shutdown = shutdown.clone();

            let handle = tokio::spawn(async move {
                spawn_kafka_event_worker(i, consumer, executor, store, event_config, shutdown)
                    .await
            });

            handles.push(handle);
        }

        // Spawn PostgreSQL handler workers (same as PostgresBackend)
        for i in 0..config.handler_workers {
            let store = self.store.clone();
            let executor = executor.clone();
            let handler_config = config.handler_worker.clone();
            let shutdown = shutdown.clone();

            let handle = tokio::spawn(async move {
                info!("Handler worker {} started", i);

                while !shutdown.is_cancelled() {
                    // Poll next handler execution from PostgreSQL
                    match store.poll_next_effect().await {
                        Ok(Some(execution)) => {
                            // Execute using JobExecutor
                            match executor
                                .execute_handler(execution.clone(), &handler_config, &DIRECT_RUNNER)
                                .await
                            {
                                Ok(result) => {
                                    match result.status {
                                        HandlerStatus::Success => {
                                            if result.emitted_events.is_empty() {
                                                if let Err(e) = store
                                                    .complete_effect(
                                                        execution.event_id,
                                                        execution.handler_id,
                                                        result.result,
                                                    )
                                                    .await
                                                {
                                                    error!("Failed to complete effect: {}", e);
                                                }
                                            } else {
                                                if let Err(e) = store
                                                    .complete_effect_with_events(
                                                        execution.event_id,
                                                        execution.handler_id,
                                                        result.result,
                                                        result.emitted_events,
                                                    )
                                                    .await
                                                {
                                                    error!(
                                                        "Failed to complete effect with events: {}",
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                        HandlerStatus::Failed { error, attempts }
                                        | HandlerStatus::Retry { error, attempts } => {
                                            if attempts >= execution.max_attempts {
                                                if let Err(e) = store
                                                    .dlq_effect(
                                                        execution.event_id,
                                                        execution.handler_id,
                                                        error,
                                                        "max_retries_exceeded".to_string(),
                                                        attempts,
                                                    )
                                                    .await
                                                {
                                                    error!("Failed to DLQ effect: {}", e);
                                                }
                                            } else {
                                                if let Err(e) = store
                                                    .fail_effect(
                                                        execution.event_id,
                                                        execution.handler_id,
                                                        error,
                                                        attempts,
                                                    )
                                                    .await
                                                {
                                                    error!("Failed to mark effect as failed: {}", e);
                                                }
                                            }
                                        }
                                        HandlerStatus::Timeout => {
                                            error!("Handler timed out: {}", execution.handler_id);
                                            if execution.attempts >= execution.max_attempts {
                                                if let Err(e) = store
                                                    .dlq_effect(
                                                        execution.event_id,
                                                        execution.handler_id,
                                                        "Handler execution timed out".to_string(),
                                                        "timeout".to_string(),
                                                        execution.attempts,
                                                    )
                                                    .await
                                                {
                                                    error!("Failed to DLQ timed out effect: {}", e);
                                                }
                                            } else {
                                                if let Err(e) = store
                                                    .fail_effect(
                                                        execution.event_id,
                                                        execution.handler_id,
                                                        "Handler execution timed out".to_string(),
                                                        execution.attempts,
                                                    )
                                                    .await
                                                {
                                                    error!(
                                                        "Failed to mark timed out effect as failed: {}",
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                        HandlerStatus::JoinWaiting => {
                                            // Complete with join_waiting status
                                            if let Err(e) = store
                                                .complete_effect(
                                                    execution.event_id,
                                                    execution.handler_id,
                                                    result.result,
                                                )
                                                .await
                                            {
                                                error!("Failed to complete join_waiting effect: {}", e);
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Handler execution failed: {}", e);
                                }
                            }
                        }
                        Ok(None) => {
                            sleep(handler_config.poll_interval).await;
                        }
                        Err(e) => {
                            error!("Error polling handlers: {}", e);
                            sleep(handler_config.poll_interval).await;
                        }
                    }
                }

                info!("Handler worker {} stopped", i);
                Ok::<_, anyhow::Error>(())
            });

            handles.push(handle);
        }

        // Wait for shutdown signal
        shutdown.cancelled().await;
        info!("Shutdown signal received, waiting for workers to finish...");

        // Wait for graceful shutdown with timeout
        let shutdown_result = tokio::time::timeout(
            config.graceful_shutdown_timeout,
            futures::future::join_all(handles),
        )
        .await;

        match shutdown_result {
            Ok(results) => {
                for (i, result) in results.into_iter().enumerate() {
                    match result {
                        Ok(Ok(())) => info!("Worker {} shut down cleanly", i),
                        Ok(Err(e)) => error!("Worker {} error during shutdown: {}", i, e),
                        Err(e) => error!("Worker {} panicked: {}", i, e),
                    }
                }
            }
            Err(_) => {
                error!(
                    "Graceful shutdown timed out after {:?}",
                    config.graceful_shutdown_timeout
                );
            }
        }

        info!("Kafka backend shut down");
        Ok(())
    }
}
