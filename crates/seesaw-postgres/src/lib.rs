// Simplified PostgresStore without compile-time checked queries
// Uses dynamic queries for easier testing

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use seesaw_core::{
    insight::*, EmittedEvent, EventProcessingCommit, ExpiredJoinWindow, JoinEntry, QueuedEvent,
    QueuedHandlerExecution, Store, NAMESPACE_SEESAW,
};
use sqlx::{FromRow, PgPool};
use std::collections::HashSet;
use uuid::Uuid;

const EVENT_CLAIM_SECONDS: i64 = 30;

fn emitted_event_created_at(parent_created_at: DateTime<Utc>) -> DateTime<Utc> {
    parent_created_at
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight should always be a valid UTC timestamp")
        .and_utc()
}

fn handler_retry_delay_seconds(attempts: i32) -> i64 {
    let exponent = attempts.saturating_sub(1).clamp(0, 8) as u32;
    1_i64 << exponent
}

/// PostgreSQL implementation of Store trait
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Clone for PostgresStore {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}

#[derive(FromRow)]
struct EventRow {
    id: i64,
    event_id: Uuid,
    parent_id: Option<Uuid>,
    correlation_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    hops: i32,
    retry_count: i32,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct HandlerRow {
    event_id: Uuid,
    handler_id: String,
    correlation_id: Uuid,
    event_type: String,
    event_payload: serde_json::Value,
    parent_event_id: Option<Uuid>,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    execute_at: DateTime<Utc>,
    timeout_seconds: i32,
    max_attempts: i32,
    priority: i32,
    attempts: i32,
    join_window_timeout_seconds: Option<i32>,
}

#[derive(FromRow)]
struct ParentEventRow {
    hops: i32,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct DlqSourceEventRow {
    correlation_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    hops: i32,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct WorkflowEventRow {
    id: i64,
    event_id: Uuid,
    correlation_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    created_at: DateTime<Utc>,
}

#[async_trait]
impl Store for PostgresStore {
    async fn publish(&self, event: QueuedEvent) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // Use the non-partitioned ledger as the idempotency guard. This keeps
        // webhook/process_with_id dedupe stable even when created_at differs.
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO seesaw_processed (event_id, correlation_id, created_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (event_id) DO NOTHING
             RETURNING event_id",
        )
        .bind(event.event_id)
        .bind(event.correlation_id)
        .bind(event.created_at)
        .fetch_optional(&mut *tx)
        .await?;

        if inserted.is_none() {
            tx.commit().await?;
            return Ok(());
        }

        sqlx::query(
            "INSERT INTO seesaw_events (
                event_id, parent_id, correlation_id, event_type, payload, hops, retry_count,
                batch_id, batch_index, batch_size, created_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(event.event_id)
        .bind(event.parent_id)
        .bind(event.correlation_id)
        .bind(event.event_type)
        .bind(event.payload)
        .bind(event.hops)
        .bind(event.retry_count)
        .bind(event.batch_id)
        .bind(event.batch_index)
        .bind(event.batch_size)
        .bind(event.created_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(())
    }

    async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        let row: Option<EventRow> = sqlx::query_as(
            "WITH next_event AS (
                SELECT e.id
                FROM seesaw_events e
                WHERE e.processed_at IS NULL
                  AND (e.locked_until IS NULL OR e.locked_until < NOW())
                  AND NOT EXISTS (
                    SELECT 1
                    FROM seesaw_events older
                    WHERE older.correlation_id = e.correlation_id
                      AND older.processed_at IS NULL
                      AND (
                        older.created_at < e.created_at
                        OR (older.created_at = e.created_at AND older.id < e.id)
                      )
                  )
                ORDER BY e.created_at ASC, e.id ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE seesaw_events e
            SET locked_until = NOW() + ($1 * INTERVAL '1 second')
            FROM next_event
            WHERE e.id = next_event.id
            RETURNING e.id, e.event_id, e.parent_id, e.correlation_id, e.event_type, e.payload,
                      e.hops, e.retry_count, e.batch_id, e.batch_index, e.batch_size, e.created_at",
        )
        .bind(EVENT_CLAIM_SECONDS)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| QueuedEvent {
            id: r.id,
            event_id: r.event_id,
            parent_id: r.parent_id,
            correlation_id: r.correlation_id,
            event_type: r.event_type,
            payload: r.payload,
            hops: r.hops,
            retry_count: r.retry_count,
            batch_id: r.batch_id,
            batch_index: r.batch_index,
            batch_size: r.batch_size,
            created_at: r.created_at,
        }))
    }

    async fn ack(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE seesaw_events SET processed_at = NOW(), locked_until = NULL WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn nack(&self, id: i64, retry_after_secs: u64) -> Result<()> {
        let locked_until = Utc::now() + Duration::seconds(retry_after_secs as i64);
        sqlx::query(
            "UPDATE seesaw_events
             SET retry_count = retry_count + 1,
                 locked_until = $2
             WHERE id = $1",
        )
        .bind(id)
        .bind(locked_until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn commit_event_processing(&self, commit: EventProcessingCommit) -> Result<()> {
        let EventProcessingCommit {
            event_row_id,
            event_id,
            correlation_id,
            event_type,
            event_payload,
            queued_effect_intents,
            inline_effect_failures,
            emitted_events,
        } = commit;

        let mut tx = self.pool.begin().await?;

        for intent in queued_effect_intents {
            sqlx::query(
                "INSERT INTO seesaw_handler_executions (
                    event_id, handler_id, correlation_id, status,
                    event_type, event_payload, parent_event_id,
                    batch_id, batch_index, batch_size,
                    execute_at, timeout_seconds, max_attempts, priority,
                    join_window_timeout_seconds
                 )
                 VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                 ON CONFLICT (event_id, handler_id) DO NOTHING",
            )
            .bind(event_id)
            .bind(intent.handler_id)
            .bind(correlation_id)
            .bind(&event_type)
            .bind(&event_payload)
            .bind(intent.parent_event_id)
            .bind(intent.batch_id)
            .bind(intent.batch_index)
            .bind(intent.batch_size)
            .bind(intent.execute_at)
            .bind(intent.timeout_seconds)
            .bind(intent.max_attempts)
            .bind(intent.priority)
            .bind(intent.join_window_timeout_seconds)
            .execute(&mut *tx)
            .await?;
        }

        for event in emitted_events {
            let inserted: Option<Uuid> = sqlx::query_scalar(
                "INSERT INTO seesaw_processed (event_id, correlation_id, created_at)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (event_id) DO NOTHING
                 RETURNING event_id",
            )
            .bind(event.event_id)
            .bind(event.correlation_id)
            .bind(event.created_at)
            .fetch_optional(&mut *tx)
            .await?;

            if inserted.is_none() {
                continue;
            }

            sqlx::query(
                "INSERT INTO seesaw_events (
                    event_id, parent_id, correlation_id, event_type, payload, hops, retry_count,
                    batch_id, batch_index, batch_size, created_at
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(event.event_id)
            .bind(event.parent_id)
            .bind(event.correlation_id)
            .bind(event.event_type)
            .bind(event.payload)
            .bind(event.hops)
            .bind(event.retry_count)
            .bind(event.batch_id)
            .bind(event.batch_index)
            .bind(event.batch_size)
            .bind(event.created_at)
            .execute(&mut *tx)
            .await?;
        }

        let source_event: Option<DlqSourceEventRow> = if inline_effect_failures.is_empty() {
            None
        } else {
            sqlx::query_as(
                "SELECT correlation_id, event_type, payload, batch_id, batch_index, batch_size, hops, created_at
                 FROM seesaw_events
                 WHERE event_id = $1
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1",
            )
            .bind(event_id)
            .fetch_optional(&mut *tx)
            .await?
        };

        for failure in inline_effect_failures {
            sqlx::query(
                "INSERT INTO seesaw_dlq (
                    event_id, handler_id, correlation_id, error, event_type, event_payload, reason, attempts
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(event_id)
            .bind(&failure.handler_id)
            .bind(correlation_id)
            .bind(&failure.error)
            .bind(&event_type)
            .bind(&event_payload)
            .bind(&failure.reason)
            .bind(failure.attempts)
            .execute(&mut *tx)
            .await?;

            if let Some(source) = source_event.as_ref() {
                if let (Some(batch_id), Some(batch_index), Some(batch_size)) =
                    (source.batch_id, source.batch_index, source.batch_size)
                {
                    let synthetic_event_id = Uuid::new_v5(
                        &NAMESPACE_SEESAW,
                        format!("{}-{}-dlq-terminal", event_id, failure.handler_id).as_bytes(),
                    );
                    let synthetic_created_at = emitted_event_created_at(source.created_at);

                    sqlx::query(
                        "INSERT INTO seesaw_events (
                            event_id, parent_id, correlation_id, event_type, payload, hops,
                            batch_id, batch_index, batch_size, created_at
                         )
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                         ON CONFLICT (event_id, created_at) DO NOTHING",
                    )
                    .bind(synthetic_event_id)
                    .bind(Some(event_id))
                    .bind(correlation_id)
                    .bind(&event_type)
                    .bind(&event_payload)
                    .bind(source.hops + 1)
                    .bind(Some(batch_id))
                    .bind(Some(batch_index))
                    .bind(Some(batch_size))
                    .bind(synthetic_created_at)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        let ack_result = sqlx::query(
            "UPDATE seesaw_events SET processed_at = NOW(), locked_until = NULL WHERE id = $1",
        )
        .bind(event_row_id)
        .execute(&mut *tx)
        .await?;
        if ack_result.rows_affected() != 1 {
            anyhow::bail!(
                "atomic event commit failed to ack source event row {}",
                event_row_id
            );
        }

        tx.commit().await?;
        Ok(())
    }

    async fn insert_effect_intent(
        &self,
        event_id: Uuid,
        handler_id: String,
        correlation_id: Uuid,
        event_type: String,
        event_payload: serde_json::Value,
        parent_event_id: Option<Uuid>,
        batch_id: Option<Uuid>,
        batch_index: Option<i32>,
        batch_size: Option<i32>,
        execute_at: DateTime<Utc>,
        timeout_seconds: i32,
        max_attempts: i32,
        priority: i32,
        join_window_timeout_seconds: Option<i32>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO seesaw_handler_executions (
                event_id, handler_id, correlation_id, status,
                event_type, event_payload, parent_event_id,
                batch_id, batch_index, batch_size,
                execute_at, timeout_seconds, max_attempts, priority,
                join_window_timeout_seconds
             )
             VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (event_id, handler_id) DO NOTHING",
        )
        .bind(event_id)
        .bind(handler_id)
        .bind(correlation_id)
        .bind(event_type)
        .bind(event_payload)
        .bind(parent_event_id)
        .bind(batch_id)
        .bind(batch_index)
        .bind(batch_size)
        .bind(execute_at)
        .bind(timeout_seconds)
        .bind(max_attempts)
        .bind(priority)
        .bind(join_window_timeout_seconds)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn poll_next_effect(&self) -> Result<Option<QueuedHandlerExecution>> {
        let row: Option<HandlerRow> = sqlx::query_as(
            "WITH next_effect AS (
                SELECT event_id, handler_id
                FROM seesaw_handler_executions
                WHERE (
                    status = 'pending'
                    OR (status = 'failed' AND attempts < max_attempts)
                )
                  AND execute_at <= NOW()
                ORDER BY priority ASC, execute_at ASC, event_id ASC, handler_id ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE seesaw_handler_executions e
            SET status = 'executing',
                claimed_at = NOW(),
                last_attempted_at = NOW(),
                attempts = e.attempts + 1
            FROM next_effect
            WHERE e.event_id = next_effect.event_id
              AND e.handler_id = next_effect.handler_id
            RETURNING
                e.event_id, e.handler_id, e.correlation_id, e.event_type, e.event_payload, e.parent_event_id,
                e.batch_id, e.batch_index, e.batch_size,
                e.execute_at, e.timeout_seconds, e.max_attempts, e.priority, e.attempts,
                e.join_window_timeout_seconds",
        )
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            Ok(Some(QueuedHandlerExecution {
                event_id: r.event_id,
                handler_id: r.handler_id,
                correlation_id: r.correlation_id,
                event_type: r.event_type,
                event_payload: r.event_payload,
                parent_event_id: r.parent_event_id,
                batch_id: r.batch_id,
                batch_index: r.batch_index,
                batch_size: r.batch_size,
                execute_at: r.execute_at,
                timeout_seconds: r.timeout_seconds,
                max_attempts: r.max_attempts,
                priority: r.priority,
                attempts: r.attempts,
                join_window_timeout_seconds: r.join_window_timeout_seconds,
            }))
        } else {
            Ok(None)
        }
    }

    async fn complete_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        result: serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE seesaw_handler_executions
             SET status = 'completed',
                 result = $3,
                 completed_at = NOW()
             WHERE event_id = $1 AND handler_id = $2",
        )
        .bind(event_id)
        .bind(handler_id)
        .bind(result)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn complete_effect_with_events(
        &self,
        event_id: Uuid,
        handler_id: String,
        result: serde_json::Value,
        emitted_events: Vec<EmittedEvent>,
    ) -> Result<()> {
        // Get correlation_id and hops for emitted events
        let effect: HandlerRow = sqlx::query_as(
            "SELECT event_id, handler_id, correlation_id, event_type, event_payload, parent_event_id,
                    batch_id, batch_index, batch_size,
                    execute_at, timeout_seconds, max_attempts, priority, attempts, join_window_timeout_seconds
             FROM seesaw_handler_executions
             WHERE event_id = $1 AND handler_id = $2",
        )
        .bind(event_id)
        .bind(&handler_id)
        .fetch_one(&self.pool)
        .await?;

        // Read parent metadata for deterministic hop increment and timestamp.
        let parent: ParentEventRow = sqlx::query_as(
            "SELECT hops, created_at
             FROM seesaw_events
             WHERE event_id = $1
             ORDER BY created_at ASC, id ASC
             LIMIT 1",
        )
        .bind(event_id)
        .fetch_one(&self.pool)
        .await?;

        // Start transaction for atomicity
        let mut tx = self.pool.begin().await?;

        // Insert emitted events with deterministic IDs
        for (emitted_index, emitted) in emitted_events.into_iter().enumerate() {
            // Generate deterministic event_id from
            // hash(parent_event_id, handler_id, event_type, emitted_index)
            let deterministic_id = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!(
                    "{}-{}-{}-{}",
                    event_id, handler_id, emitted.event_type, emitted_index
                )
                .as_bytes(),
            );

            // Deterministic timestamp keeps retries idempotent while staying in
            // the same partition day as the parent event.
            let deterministic_timestamp = emitted_event_created_at(parent.created_at);

            // Insert event (idempotent via ON CONFLICT on (event_id, created_at))
            sqlx::query(
                "INSERT INTO seesaw_events (
                    event_id, parent_id, correlation_id, event_type, payload, hops,
                    batch_id, batch_index, batch_size, created_at
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (event_id, created_at) DO NOTHING",
            )
            .bind(deterministic_id)
            .bind(Some(event_id))
            .bind(effect.correlation_id)
            .bind(&emitted.event_type)
            .bind(emitted.payload)
            .bind(parent.hops + 1)
            .bind(emitted.batch_id)
            .bind(emitted.batch_index)
            .bind(emitted.batch_size)
            .bind(deterministic_timestamp)
            .execute(&mut *tx)
            .await?;
        }

        // Mark effect as completed (same transaction)
        sqlx::query(
            "UPDATE seesaw_handler_executions
             SET status = 'completed',
                 result = $3,
                 completed_at = NOW()
             WHERE event_id = $1 AND handler_id = $2",
        )
        .bind(event_id)
        .bind(handler_id)
        .bind(result)
        .execute(&mut *tx)
        .await?;

        // Commit transaction - both succeed or both fail
        tx.commit().await?;

        Ok(())
    }

    async fn fail_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        attempts: i32,
    ) -> Result<()> {
        let retry_at = Utc::now() + Duration::seconds(handler_retry_delay_seconds(attempts));
        sqlx::query(
            "UPDATE seesaw_handler_executions
             SET status = 'failed',
                 error = $3,
                 execute_at = $5,
                 claimed_at = NULL
             WHERE event_id = $1 AND handler_id = $2 AND attempts >= $4",
        )
        .bind(event_id)
        .bind(handler_id)
        .bind(error)
        .bind(attempts)
        .bind(retry_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn dlq_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        reason: String,
        attempts: i32,
    ) -> Result<()> {
        self.dlq_effect_with_events(event_id, handler_id, error, reason, attempts, Vec::new())
            .await
    }

    async fn dlq_effect_with_events(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        reason: String,
        attempts: i32,
        emitted_events: Vec<EmittedEvent>,
    ) -> Result<()> {
        // Effect details may be missing for inline/synthetic failures. Fall back to
        // parent event data so DLQ writes still succeed.
        let effect = sqlx::query_as::<_, HandlerRow>(
            "SELECT event_id, handler_id, correlation_id, event_type, event_payload, parent_event_id,
                    batch_id, batch_index, batch_size,
                    execute_at, timeout_seconds, max_attempts, priority, attempts, join_window_timeout_seconds
             FROM seesaw_handler_executions
             WHERE event_id = $1 AND handler_id = $2",
        )
        .bind(event_id)
        .bind(&handler_id)
        .fetch_optional(&self.pool)
        .await?;

        let source_event = sqlx::query_as::<_, DlqSourceEventRow>(
            "SELECT correlation_id, event_type, payload, batch_id, batch_index, batch_size, hops, created_at
             FROM seesaw_events
             WHERE event_id = $1
             ORDER BY created_at ASC, id ASC
             LIMIT 1",
        )
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;

        let (
            source_correlation_id,
            source_event_type,
            source_event_payload,
            source_batch_id,
            source_batch_index,
            source_batch_size,
        ) = if let Some(effect) = effect {
            (
                effect.correlation_id,
                effect.event_type,
                effect.event_payload,
                effect.batch_id,
                effect.batch_index,
                effect.batch_size,
            )
        } else if let Some(source) = source_event.as_ref() {
            (
                source.correlation_id,
                source.event_type.clone(),
                source.payload.clone(),
                source.batch_id,
                source.batch_index,
                source.batch_size,
            )
        } else {
            anyhow::bail!(
                "cannot DLQ unknown effect {} for missing event {}",
                handler_id,
                event_id
            );
        };

        let mut tx = self.pool.begin().await?;

        // Insert into DLQ
        sqlx::query(
            "INSERT INTO seesaw_dlq (
                event_id, handler_id, correlation_id, error, event_type, event_payload, reason, attempts
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(event_id)
        .bind(&handler_id)
        .bind(source_correlation_id)
        .bind(&error)
        .bind(&source_event_type)
        .bind(&source_event_payload)
        .bind(&reason)
        .bind(attempts)
        .execute(&mut *tx)
        .await?;
        let preserve_batch_terminal = reason != "accumulate_timeout";

        let synthetic_created_at = source_event
            .as_ref()
            .map(|row| emitted_event_created_at(row.created_at))
            .unwrap_or_else(Utc::now);
        let synthetic_hops = source_event.as_ref().map(|row| row.hops + 1).unwrap_or(0);

        if emitted_events.is_empty() {
            if !preserve_batch_terminal {
                sqlx::query(
                    "DELETE FROM seesaw_handler_executions WHERE event_id = $1 AND handler_id = $2",
                )
                .bind(event_id)
                .bind(&handler_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(());
            }
            if let (Some(batch_id), Some(batch_index), Some(batch_size)) =
                (source_batch_id, source_batch_index, source_batch_size)
            {
                let synthetic_event_id = Uuid::new_v5(
                    &NAMESPACE_SEESAW,
                    format!("{}-{}-dlq-terminal", event_id, handler_id).as_bytes(),
                );

                sqlx::query(
                    "INSERT INTO seesaw_events (
                        event_id, parent_id, correlation_id, event_type, payload, hops,
                        batch_id, batch_index, batch_size, created_at
                     )
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                     ON CONFLICT (event_id, created_at) DO NOTHING",
                )
                .bind(synthetic_event_id)
                .bind(Some(event_id))
                .bind(source_correlation_id)
                .bind(&source_event_type)
                .bind(&source_event_payload)
                .bind(synthetic_hops)
                .bind(Some(batch_id))
                .bind(Some(batch_index))
                .bind(Some(batch_size))
                .bind(synthetic_created_at)
                .execute(&mut *tx)
                .await?;
            }
        } else {
            for (emitted_index, emitted) in emitted_events.into_iter().enumerate() {
                let synthetic_event_id = Uuid::new_v5(
                    &NAMESPACE_SEESAW,
                    format!(
                        "{}-{}-dlq-terminal-{}-{}",
                        event_id, handler_id, emitted.event_type, emitted_index
                    )
                    .as_bytes(),
                );

                sqlx::query(
                    "INSERT INTO seesaw_events (
                        event_id, parent_id, correlation_id, event_type, payload, hops,
                        batch_id, batch_index, batch_size, created_at
                     )
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                     ON CONFLICT (event_id, created_at) DO NOTHING",
                )
                .bind(synthetic_event_id)
                .bind(Some(event_id))
                .bind(source_correlation_id)
                .bind(&emitted.event_type)
                .bind(emitted.payload)
                .bind(synthetic_hops)
                .bind(emitted.batch_id.or(if preserve_batch_terminal {
                    source_batch_id
                } else {
                    None
                }))
                .bind(emitted.batch_index.or(if preserve_batch_terminal {
                    source_batch_index
                } else {
                    None
                }))
                .bind(emitted.batch_size.or(if preserve_batch_terminal {
                    source_batch_size
                } else {
                    None
                }))
                .bind(synthetic_created_at)
                .execute(&mut *tx)
                .await?;
            }
        }

        // Delete from executions table
        sqlx::query(
            "DELETE FROM seesaw_handler_executions WHERE event_id = $1 AND handler_id = $2",
        )
        .bind(event_id)
        .bind(&handler_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(())
    }

    async fn subscribe_workflow_events(
        &self,
        correlation_id: Uuid,
    ) -> Result<Box<dyn futures::Stream<Item = seesaw_core::WorkflowEvent> + Send + Unpin>> {
        use sqlx::postgres::PgListener;

        let channel = format!("seesaw_workflow_{}", correlation_id);
        const PAGE_SIZE: i64 = 256;
        const CATCH_UP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

        // Establish a cursor at subscribe time so callers only receive new
        // workflow events emitted after this subscription starts.
        let initial_cursor: Option<(DateTime<Utc>, i64)> = sqlx::query_as(
            "SELECT created_at, id
             FROM seesaw_events
             WHERE correlation_id = $1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?;

        // Create a new listener connection
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen(&channel).await?;

        let pool = self.pool.clone();
        let (tx, rx) = futures::channel::mpsc::unbounded::<seesaw_core::WorkflowEvent>();

        tokio::spawn(async move {
            let mut cursor = initial_cursor;
            let mut drain_pending = true;

            loop {
                if !drain_pending {
                    match tokio::time::timeout(CATCH_UP_INTERVAL, listener.recv()).await {
                        Ok(Ok(_notification)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(
                                "workflow listener recv failed for {}: {}",
                                correlation_id,
                                error
                            );
                            return;
                        }
                        Err(_) => {}
                    }
                }
                drain_pending = false;

                loop {
                    let rows_result: std::result::Result<Vec<WorkflowEventRow>, sqlx::Error> =
                        if let Some((created_at, id)) = cursor {
                            sqlx::query_as(
                                "SELECT id, event_id, correlation_id, event_type, payload, created_at
                                 FROM seesaw_events
                                 WHERE correlation_id = $1
                                   AND (
                                        created_at > $2
                                        OR (created_at = $2 AND id > $3)
                                   )
                                 ORDER BY created_at ASC, id ASC
                                 LIMIT $4",
                            )
                            .bind(correlation_id)
                            .bind(created_at)
                            .bind(id)
                            .bind(PAGE_SIZE)
                            .fetch_all(&pool)
                            .await
                        } else {
                            sqlx::query_as(
                                "SELECT id, event_id, correlation_id, event_type, payload, created_at
                                 FROM seesaw_events
                                 WHERE correlation_id = $1
                                 ORDER BY created_at ASC, id ASC
                                 LIMIT $2",
                            )
                            .bind(correlation_id)
                            .bind(PAGE_SIZE)
                            .fetch_all(&pool)
                            .await
                        };

                    let rows = match rows_result {
                        Ok(rows) => rows,
                        Err(error) => {
                            tracing::warn!(
                                "workflow event query failed for {}: {}",
                                correlation_id,
                                error
                            );
                            return;
                        }
                    };

                    if rows.is_empty() {
                        break;
                    }

                    for row in rows {
                        cursor = Some((row.created_at, row.id));
                        if tx
                            .unbounded_send(seesaw_core::WorkflowEvent {
                                event_id: row.event_id,
                                correlation_id: row.correlation_id,
                                event_type: row.event_type,
                                payload: row.payload,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        });

        Ok(Box::new(rx))
    }

    async fn get_workflow_status(
        &self,
        correlation_id: Uuid,
    ) -> Result<seesaw_core::WorkflowStatus> {
        let pending_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_handler_executions
             WHERE correlation_id = $1 AND status IN ('pending', 'executing', 'failed')",
        )
        .bind(correlation_id)
        .fetch_one(&self.pool)
        .await?
        .0;

        let last_event = sqlx::query_as::<_, (String,)>(
            "SELECT event_type FROM seesaw_events
             WHERE correlation_id = $1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|r| r.0);

        Ok(seesaw_core::WorkflowStatus {
            correlation_id,
            pending_effects,
            is_settled: pending_effects == 0,
            last_event,
        })
    }

    async fn join_same_batch_append_and_maybe_claim(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        source_event_id: Uuid,
        source_event_type: String,
        source_payload: serde_json::Value,
        source_created_at: DateTime<Utc>,
        batch_id: Uuid,
        batch_index: i32,
        batch_size: i32,
        join_window_timeout_seconds: Option<i32>,
    ) -> Result<Option<Vec<JoinEntry>>> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO seesaw_join_entries (
                join_handler_id, correlation_id, source_event_id, source_event_type, source_payload,
                source_created_at, batch_id, batch_index, batch_size
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (join_handler_id, correlation_id, source_event_id) DO NOTHING",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(source_event_id)
        .bind(&source_event_type)
        .bind(source_payload)
        .bind(source_created_at)
        .bind(batch_id)
        .bind(batch_index)
        .bind(batch_size)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO seesaw_join_windows (
                join_handler_id, correlation_id, mode, batch_id, target_count, status,
                window_timeout_seconds, expires_at
             )
             VALUES (
                $1,
                $2,
                'same_batch',
                $3,
                $4,
                'open',
                $5,
                CASE
                    WHEN $5 IS NULL THEN NULL
                    ELSE NOW() + ($5::int * INTERVAL '1 second')
                END
             )
             ON CONFLICT (join_handler_id, correlation_id, batch_id) DO NOTHING",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .bind(batch_size)
        .bind(join_window_timeout_seconds)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE seesaw_join_windows
             SET target_count = $4,
                 updated_at = NOW()
             WHERE join_handler_id = $1
               AND correlation_id = $2
               AND batch_id = $3
               AND target_count <> $4",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .bind(batch_size)
        .execute(&mut *tx)
        .await?;

        let claimed: Option<(String,)> = sqlx::query_as(
            "UPDATE seesaw_join_windows w
             SET status = 'processing',
                 sealed_at = COALESCE(w.sealed_at, NOW()),
                 processing_started_at = NOW(),
                 updated_at = NOW(),
                 last_error = NULL
             WHERE w.join_handler_id = $1
               AND w.correlation_id = $2
               AND w.batch_id = $3
               AND w.status = 'open'
               AND (w.expires_at IS NULL OR w.expires_at > NOW())
               AND (
                    SELECT COUNT(*)::int
                    FROM seesaw_join_entries e
                    WHERE e.join_handler_id = w.join_handler_id
                      AND e.correlation_id = w.correlation_id
                      AND e.batch_id = w.batch_id
               ) >= w.target_count
             RETURNING w.join_handler_id",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .fetch_optional(&mut *tx)
        .await?;

        if claimed.is_none() {
            tx.commit().await?;
            return Ok(None);
        }

        let rows = sqlx::query_as::<_, (Uuid, String, serde_json::Value, Uuid, i32, i32, DateTime<Utc>)>(
            "SELECT source_event_id, source_event_type, source_payload, batch_id, batch_index, batch_size, source_created_at
             FROM seesaw_join_entries
             WHERE join_handler_id = $1
               AND correlation_id = $2
               AND batch_id = $3
             ORDER BY batch_index ASC, source_created_at ASC, source_event_id ASC",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .fetch_all(&mut *tx)
        .await?;

        let entries = rows
            .into_iter()
            .map(
                |(
                    source_event_id,
                    event_type,
                    payload,
                    batch_id,
                    batch_index,
                    batch_size,
                    created_at,
                )| JoinEntry {
                    source_event_id,
                    event_type,
                    payload,
                    batch_id,
                    batch_index,
                    batch_size,
                    created_at,
                },
            )
            .collect::<Vec<_>>();

        tx.commit().await?;
        Ok(Some(entries))
    }

    async fn join_same_batch_complete(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE seesaw_join_windows
             SET status = 'completed',
                 completed_at = NOW(),
                 updated_at = NOW()
             WHERE join_handler_id = $1
               AND correlation_id = $2
               AND batch_id = $3",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM seesaw_join_entries
             WHERE join_handler_id = $1
               AND correlation_id = $2
               AND batch_id = $3",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM seesaw_join_windows
             WHERE join_handler_id = $1
               AND correlation_id = $2
               AND batch_id = $3",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn join_same_batch_release(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
        error: String,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE seesaw_join_windows
             SET status = 'open',
                 processing_started_at = NULL,
                 last_error = $4,
                 updated_at = NOW()
             WHERE join_handler_id = $1
               AND correlation_id = $2
               AND batch_id = $3
               AND status = 'processing'",
        )
        .bind(&join_handler_id)
        .bind(correlation_id)
        .bind(batch_id)
        .bind(error)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn expire_same_batch_windows(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredJoinWindow>> {
        let mut tx = self.pool.begin().await?;

        let windows = sqlx::query_as::<_, (String, Uuid, Uuid)>(
            "SELECT join_handler_id, correlation_id, batch_id
             FROM seesaw_join_windows
             WHERE status = 'open'
               AND expires_at IS NOT NULL
               AND expires_at <= $1
             FOR UPDATE SKIP LOCKED",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;

        let mut expired = Vec::with_capacity(windows.len());
        for (join_handler_id, correlation_id, batch_id) in windows {
            let source_event_ids = sqlx::query_as::<_, (Uuid,)>(
                "SELECT source_event_id
                 FROM seesaw_join_entries
                 WHERE join_handler_id = $1
                   AND correlation_id = $2
                   AND batch_id = $3
                 ORDER BY batch_index ASC, source_created_at ASC, source_event_id ASC",
            )
            .bind(&join_handler_id)
            .bind(correlation_id)
            .bind(batch_id)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|row| row.0)
            .collect::<Vec<_>>();

            sqlx::query(
                "DELETE FROM seesaw_join_entries
                 WHERE join_handler_id = $1
                   AND correlation_id = $2
                   AND batch_id = $3",
            )
            .bind(&join_handler_id)
            .bind(correlation_id)
            .bind(batch_id)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "DELETE FROM seesaw_join_windows
                 WHERE join_handler_id = $1
                   AND correlation_id = $2
                   AND batch_id = $3",
            )
            .bind(&join_handler_id)
            .bind(correlation_id)
            .bind(batch_id)
            .execute(&mut *tx)
            .await?;

            expired.push(ExpiredJoinWindow {
                join_handler_id,
                correlation_id,
                batch_id,
                source_event_ids,
            });
        }

        tx.commit().await?;
        Ok(expired)
    }
}

#[derive(FromRow)]
struct StreamRow {
    seq: i64,
    stream_type: String,
    correlation_id: Uuid,
    event_id: Option<Uuid>,
    effect_event_id: Option<Uuid>,
    handler_id: Option<String>,
    status: Option<String>,
    error: Option<String>,
    payload: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct EffectLogRow {
    correlation_id: Uuid,
    event_id: Uuid,
    handler_id: String,
    status: String,
    attempts: i32,
    event_type: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    execute_at: DateTime<Utc>,
    claimed_at: Option<DateTime<Utc>>,
    last_attempted_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct DeadLetterRow {
    correlation_id: Uuid,
    event_id: Uuid,
    handler_id: String,
    event_type: String,
    event_payload: serde_json::Value,
    error: String,
    reason: String,
    attempts: i32,
    failed_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct FailedWorkflowRow {
    correlation_id: Uuid,
    failed_effects: i64,
    active_effects: i64,
    dead_letters: i64,
    last_failed_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

#[async_trait]
impl InsightStore for PostgresStore {
    async fn subscribe_events(
        &self,
    ) -> Result<Box<dyn futures::Stream<Item = InsightEvent> + Send + Unpin>> {
        use futures::stream::StreamExt;
        use sqlx::postgres::PgListener;

        // Create a new listener connection
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("seesaw_stream").await?;

        // Convert listener into a stream of InsightEvent
        let pool = self.pool.clone();
        let stream = listener.into_stream().filter_map(move |result| {
            let pool = pool.clone();
            Box::pin(async move {
                match result {
                    Ok(_notification) => {
                        // Fetch latest entry from stream table
                        // (notification payload is just correlation_id for wake-up)
                        if let Ok(row) = sqlx::query_as::<_, StreamRow>(
                            "SELECT seq, stream_type, correlation_id, event_id, effect_event_id,
                                    handler_id, status, error, payload, created_at
                             FROM seesaw_stream
                             ORDER BY seq DESC
                             LIMIT 1",
                        )
                        .fetch_one(&pool)
                        .await
                        {
                            Some(stream_row_to_insight_event(row))
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            })
        });

        Ok(Box::new(stream))
    }

    async fn get_workflow_tree(&self, correlation_id: Uuid) -> Result<WorkflowTree> {
        // Get all events for this correlation
        let events = sqlx::query_as::<_, EventRow>(
            "SELECT id, event_id, parent_id, correlation_id, event_type, payload, hops,
                    batch_id, batch_index, batch_size, created_at
             FROM seesaw_events
             WHERE correlation_id = $1
             ORDER BY created_at ASC",
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        // Get all effects for this correlation
        let effects = sqlx::query_as::<_, EffectTreeRow>(
            "SELECT event_id, handler_id, status, result, error, attempts, created_at,
                    batch_id, batch_index, batch_size
             FROM seesaw_handler_executions
             WHERE correlation_id = $1
             ORDER BY created_at ASC",
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        // Build tree structure
        let event_ids: HashSet<Uuid> = events.iter().map(|event| event.event_id).collect();
        let roots = build_event_tree(&events, &effects, None, &event_ids, true);

        Ok(WorkflowTree {
            correlation_id,
            roots,
            event_count: events.len(),
            effect_count: effects.len(),
        })
    }

    async fn get_stats(&self) -> Result<InsightStats> {
        let total_events = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM seesaw_events")
            .fetch_one(&self.pool)
            .await?
            .0;

        let active_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_handler_executions
             WHERE status IN ('pending', 'executing')",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        let completed_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_handler_executions WHERE status = 'completed'",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        let failed_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_handler_executions WHERE status = 'failed'",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        Ok(InsightStats {
            total_events,
            active_effects,
            completed_effects,
            failed_effects,
        })
    }

    async fn get_recent_events(
        &self,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<Vec<InsightEvent>> {
        let rows = if let Some(cursor_seq) = cursor {
            sqlx::query_as::<_, StreamRow>(
                "SELECT seq, stream_type, correlation_id, event_id, effect_event_id,
                        handler_id, status, error, payload, created_at
                 FROM seesaw_stream
                 WHERE seq > $1
                 ORDER BY seq ASC
                 LIMIT $2",
            )
            .bind(cursor_seq)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, StreamRow>(
                "SELECT seq, stream_type, correlation_id, event_id, effect_event_id,
                        handler_id, status, error, payload, created_at
                 FROM seesaw_stream
                 ORDER BY seq DESC
                 LIMIT $1",
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows.into_iter().map(stream_row_to_insight_event).collect())
    }

    async fn get_effect_logs(
        &self,
        correlation_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<EffectExecutionLog>> {
        let rows = sqlx::query_as::<_, EffectLogRow>(
            "SELECT
                correlation_id,
                event_id,
                handler_id,
                status,
                attempts,
                event_type,
                result,
                error,
                created_at,
                execute_at,
                claimed_at,
                last_attempted_at,
                completed_at
             FROM seesaw_handler_executions
             WHERE ($1::uuid IS NULL OR correlation_id = $1)
             ORDER BY COALESCE(last_attempted_at, created_at) DESC, event_id DESC
             LIMIT $2",
        )
        .bind(correlation_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let started_at = row.claimed_at.or(row.last_attempted_at);
                let duration_ms = match (started_at, row.completed_at) {
                    (Some(start), Some(end)) => Some((end - start).num_milliseconds().max(0)),
                    _ => None,
                };

                EffectExecutionLog {
                    correlation_id: row.correlation_id,
                    event_id: row.event_id,
                    handler_id: row.handler_id,
                    status: row.status,
                    attempts: row.attempts,
                    event_type: Some(row.event_type),
                    result: row.result,
                    error: row.error,
                    created_at: row.created_at,
                    execute_at: Some(row.execute_at),
                    claimed_at: row.claimed_at,
                    last_attempted_at: row.last_attempted_at,
                    completed_at: row.completed_at,
                    duration_ms,
                }
            })
            .collect())
    }

    async fn get_dead_letters(
        &self,
        unresolved_only: bool,
        limit: usize,
    ) -> Result<Vec<DeadLetterEntry>> {
        let rows = sqlx::query_as::<_, DeadLetterRow>(
            "SELECT
                correlation_id,
                event_id,
                handler_id,
                event_type,
                event_payload,
                error,
                reason,
                attempts,
                failed_at,
                resolved_at
             FROM seesaw_dlq
             WHERE (NOT $1 OR resolved_at IS NULL)
             ORDER BY failed_at DESC
             LIMIT $2",
        )
        .bind(unresolved_only)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| DeadLetterEntry {
                correlation_id: row.correlation_id,
                event_id: row.event_id,
                handler_id: row.handler_id,
                event_type: row.event_type,
                event_payload: row.event_payload,
                error: row.error,
                reason: row.reason,
                attempts: row.attempts,
                failed_at: row.failed_at,
                resolved_at: row.resolved_at,
            })
            .collect())
    }

    async fn get_failed_workflows(&self, limit: usize) -> Result<Vec<FailedWorkflow>> {
        let rows = sqlx::query_as::<_, FailedWorkflowRow>(
            "WITH effect_agg AS (
                SELECT
                    correlation_id,
                    COUNT(*) FILTER (WHERE status = 'failed')::BIGINT AS failed_effects,
                    COUNT(*) FILTER (WHERE status IN ('pending', 'executing'))::BIGINT AS active_effects,
                    MAX(last_attempted_at) FILTER (WHERE status = 'failed') AS last_failed_at,
                    MAX(error) FILTER (WHERE status = 'failed') AS last_error
                FROM seesaw_handler_executions
                GROUP BY correlation_id
             ),
             dlq_agg AS (
                SELECT
                    correlation_id,
                    COUNT(*) FILTER (WHERE resolved_at IS NULL)::BIGINT AS dead_letters,
                    MAX(failed_at) FILTER (WHERE resolved_at IS NULL) AS last_dlq_at,
                    MAX(error) FILTER (WHERE resolved_at IS NULL) AS last_dlq_error
                FROM seesaw_dlq
                GROUP BY correlation_id
             )
             SELECT
                COALESCE(e.correlation_id, d.correlation_id) AS correlation_id,
                COALESCE(e.failed_effects, 0) AS failed_effects,
                COALESCE(e.active_effects, 0) AS active_effects,
                COALESCE(d.dead_letters, 0) AS dead_letters,
                GREATEST(e.last_failed_at, d.last_dlq_at) AS last_failed_at,
                COALESCE(d.last_dlq_error, e.last_error) AS last_error
             FROM effect_agg e
             FULL OUTER JOIN dlq_agg d ON d.correlation_id = e.correlation_id
             WHERE COALESCE(e.failed_effects, 0) > 0 OR COALESCE(d.dead_letters, 0) > 0
             ORDER BY last_failed_at DESC NULLS LAST
             LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| FailedWorkflow {
                correlation_id: row.correlation_id,
                failed_effects: row.failed_effects,
                active_effects: row.active_effects,
                dead_letters: row.dead_letters,
                last_failed_at: row.last_failed_at,
                last_error: row.last_error,
            })
            .collect())
    }
}

#[derive(FromRow)]
struct EffectTreeRow {
    event_id: Uuid,
    handler_id: String,
    status: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
    attempts: i32,
    created_at: DateTime<Utc>,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
}

fn stream_row_to_insight_event(row: StreamRow) -> InsightEvent {
    let stream_type = match row.stream_type.as_str() {
        "event_dispatched" => StreamType::EventDispatched,
        "effect_started" => StreamType::EffectStarted,
        "effect_completed" => StreamType::EffectCompleted,
        "effect_failed" => StreamType::EffectFailed,
        _ => StreamType::EventDispatched, // Default fallback
    };

    // Extract event_type from payload if it's an event
    let event_type = if stream_type == StreamType::EventDispatched {
        row.payload
            .as_ref()
            .and_then(|p| p.get("event_type"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };

    InsightEvent {
        seq: row.seq,
        stream_type,
        correlation_id: row.correlation_id,
        event_id: row.event_id,
        effect_event_id: row.effect_event_id,
        handler_id: row.handler_id,
        event_type,
        status: row.status,
        error: row.error,
        payload: row.payload,
        created_at: row.created_at,
    }
}

fn build_event_tree(
    events: &[EventRow],
    effects: &[EffectTreeRow],
    parent_id: Option<Uuid>,
    event_ids: &HashSet<Uuid>,
    is_root_pass: bool,
) -> Vec<EventNode> {
    events
        .iter()
        .filter(|event| {
            if is_root_pass {
                event.parent_id.is_none()
                    || event
                        .parent_id
                        .map(|parent| !event_ids.contains(&parent))
                        .unwrap_or(false)
            } else {
                event.parent_id == parent_id
            }
        })
        .map(|event| {
            // Get effects for this event
            let event_effects: Vec<HandlerNode> = effects
                .iter()
                .filter(|eff| eff.event_id == event.event_id)
                .map(|eff| HandlerNode {
                    handler_id: eff.handler_id.clone(),
                    event_id: eff.event_id,
                    status: eff.status.clone(),
                    result: eff.result.clone(),
                    error: eff.error.clone(),
                    attempts: eff.attempts,
                    created_at: eff.created_at,
                    batch_id: eff.batch_id,
                    batch_index: eff.batch_index,
                    batch_size: eff.batch_size,
                })
                .collect();

            // Recursively build children
            let children =
                build_event_tree(events, effects, Some(event.event_id), event_ids, false);

            EventNode {
                event_id: event.event_id,
                event_type: event.event_type.clone(),
                payload: event.payload.clone(),
                created_at: event.created_at,
                batch_id: event.batch_id,
                batch_index: event.batch_index,
                batch_size: event.batch_size,
                children,
                effects: event_effects,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    #[test]
    fn emitted_event_created_at_is_midnight_on_parent_day() {
        let parent = Utc
            .with_ymd_and_hms(2026, 2, 5, 18, 45, 12)
            .single()
            .expect("valid timestamp");

        let emitted = emitted_event_created_at(parent);

        assert_eq!(emitted.date_naive(), parent.date_naive());
        assert_eq!(emitted.hour(), 0);
        assert_eq!(emitted.minute(), 0);
        assert_eq!(emitted.second(), 0);
    }

    #[test]
    fn emitted_event_created_at_is_deterministic_for_same_parent_day() {
        let first_parent = Utc
            .with_ymd_and_hms(2026, 2, 5, 0, 1, 2)
            .single()
            .expect("valid timestamp");
        let second_parent = Utc
            .with_ymd_and_hms(2026, 2, 5, 23, 59, 59)
            .single()
            .expect("valid timestamp");

        let first_emitted = emitted_event_created_at(first_parent);
        let second_emitted = emitted_event_created_at(second_parent);

        assert_eq!(first_emitted, second_emitted);
    }

    #[test]
    fn handler_retry_delay_seconds_uses_exponential_backoff() {
        assert_eq!(handler_retry_delay_seconds(1), 1);
        assert_eq!(handler_retry_delay_seconds(2), 2);
        assert_eq!(handler_retry_delay_seconds(3), 4);
        assert_eq!(handler_retry_delay_seconds(4), 8);
    }

    #[test]
    fn handler_retry_delay_seconds_is_capped() {
        assert_eq!(handler_retry_delay_seconds(9), 256);
        assert_eq!(handler_retry_delay_seconds(50), 256);
    }

    #[test]
    fn build_event_tree_treats_orphan_parent_as_root() {
        let correlation_id = Uuid::new_v4();
        let event_id = Uuid::new_v4();
        let missing_parent = Uuid::new_v4();
        let now = Utc::now();

        let events = vec![EventRow {
            id: 1,
            event_id,
            parent_id: Some(missing_parent),
            correlation_id,
            event_type: "OrphanEvent".to_string(),
            payload: serde_json::json!({"ok": true}),
            hops: 1,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: now,
        }];

        let effects: Vec<EffectTreeRow> = Vec::new();
        let event_ids: HashSet<Uuid> = events.iter().map(|event| event.event_id).collect();

        let roots = build_event_tree(&events, &effects, None, &event_ids, true);

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].event_id, event_id);
    }
}
