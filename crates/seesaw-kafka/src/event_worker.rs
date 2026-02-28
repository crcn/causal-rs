//! Kafka event worker implementation
//!
//! This module implements the event worker loop that:
//! 1. Polls events from Kafka
//! 2. Checks idempotency in PostgreSQL
//! 3. Executes event via JobExecutor
//! 4. Commits to PostgreSQL atomically
//! 5. Commits Kafka offset (exactly-once semantics)

use anyhow::Result;
use rdkafka::consumer::StreamConsumer;
use seesaw_core::backend::job_executor::JobExecutor;
use seesaw_core::runtime::event_worker::EventWorkerConfig;
use seesaw_core::{DirectRunner, EventProcessingCommit, InlineHandlerFailure, QueuedEvent, Store};

static DIRECT_RUNNER: DirectRunner = DirectRunner;
use seesaw_postgres::PostgresStore;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::kafka_client::{commit_message_offset, deserialize_event_message, LoggingConsumerContext};

/// Spawn a Kafka event worker.
///
/// This worker polls events from Kafka, checks idempotency, processes them,
/// and commits both to PostgreSQL and Kafka for exactly-once semantics.
pub async fn spawn_kafka_event_worker<D>(
    worker_id: usize,
    consumer: StreamConsumer<LoggingConsumerContext>,
    executor: Arc<JobExecutor<D>>,
    store: PostgresStore,
    config: EventWorkerConfig,
    shutdown: CancellationToken,
) -> Result<()>
where
    D: Send + Sync + 'static,
{
    info!("Kafka event worker {} started", worker_id);

    while !shutdown.is_cancelled() {
        // Poll message from Kafka with timeout
        match tokio::time::timeout(
            std::time::Duration::from_millis(1000),
            consumer.recv()
        ).await {
            Ok(Ok(borrowed_msg)) => {
                // Convert to owned message for lifetime management
                let msg = borrowed_msg.detach();

                // Deserialize event
                let kafka_event = match deserialize_event_message(&msg) {
                    Ok(event) => event,
                    Err(e) => {
                        error!("Failed to deserialize Kafka message: {}, skipping", e);
                        // Commit offset to skip bad message
                        if let Err(e) = commit_message_offset(&consumer, &msg).await {
                            error!("Failed to commit offset for bad message: {}", e);
                        }
                        continue;
                    }
                };

                debug!(
                    "Worker {} received event {} (type: {})",
                    worker_id, kafka_event.event_id, kafka_event.event_type
                );

                // Check idempotency
                match store.is_processed(kafka_event.event_id).await {
                    Ok(true) => {
                        debug!(
                            "Event {} already processed, skipping (idempotency check)",
                            kafka_event.event_id
                        );
                        // Commit offset since we've already processed this
                        if let Err(e) = commit_message_offset(&consumer, &msg).await {
                            error!("Failed to commit offset after idempotency check: {}", e);
                        }
                        continue;
                    }
                    Ok(false) => {
                        // Not processed, continue
                    }
                    Err(e) => {
                        error!("Failed to check idempotency for event {}: {}", kafka_event.event_id, e);
                        // Don't commit offset - let Kafka redeliver
                        continue;
                    }
                }

                // Convert Kafka event to QueuedEvent for executor
                let queued_event = QueuedEvent {
                    id: 0, // Not used in Kafka backend (we use event_id for idempotency)
                    event_id: kafka_event.event_id,
                    parent_id: kafka_event.parent_id,
                    correlation_id: kafka_event.correlation_id,
                    event_type: kafka_event.event_type.clone(),
                    payload: kafka_event.payload,
                    hops: kafka_event.hops,
                    retry_count: kafka_event.retry_count,
                    batch_id: kafka_event.batch_id,
                    batch_index: kafka_event.batch_index,
                    batch_size: kafka_event.batch_size,
                    created_at: kafka_event.created_at,
                };

                // Execute event via JobExecutor
                match executor.execute_event(&queued_event, &config, &DIRECT_RUNNER).await {
                    Ok(job_commit) => {
                        // Convert JobCommit to Store's EventProcessingCommit
                        let store_commit = EventProcessingCommit {
                            event_row_id: 0, // Not used in Kafka backend
                            event_id: job_commit.event_id,
                            correlation_id: job_commit.correlation_id,
                            event_type: job_commit.event_type,
                            event_payload: job_commit.event_payload,
                            queued_effect_intents: job_commit.queued_effect_intents,
                            inline_effect_failures: job_commit
                                .inline_effect_failures
                                .into_iter()
                                .map(|f| InlineHandlerFailure {
                                    handler_id: f.handler_id,
                                    error: f.error,
                                    reason: f.reason,
                                    attempts: f.attempts,
                                })
                                .collect(),
                            emitted_events: job_commit.emitted_events,
                        };

                        // Commit to PostgreSQL atomically
                        if let Err(e) = store.commit_event_processing(store_commit).await {
                            error!(
                                "Failed to commit event processing to PostgreSQL for event {}: {}",
                                queued_event.event_id, e
                            );
                            // Don't commit Kafka offset - let it redeliver
                            continue;
                        }

                        // Only commit Kafka offset after PostgreSQL commit succeeds
                        if let Err(e) = commit_message_offset(&consumer, &msg).await {
                            error!(
                                "Failed to commit Kafka offset for event {}: {}",
                                queued_event.event_id, e
                            );
                            // PostgreSQL commit succeeded but Kafka offset failed
                            // This is OK - idempotency will catch redelivery
                        } else {
                            debug!("Successfully processed and committed event {}", queued_event.event_id);
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Event processing failed for event {}: {}",
                            queued_event.event_id, e
                        );
                        // Don't commit offset - let Kafka redeliver
                        // TODO: Consider adding retry limit and DLQ for permanently failed events
                    }
                }
            }
            Ok(Err(e)) => {
                // Kafka error
                error!("Kafka consumer error: {}", e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(_) => {
                // Timeout - no messages available
                continue;
            }
        }
    }

    info!("Kafka event worker {} stopped", worker_id);
    Ok(())
}
