// Simplified PostgresStore without compile-time checked queries
// Uses dynamic queries for easier testing

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use seesaw_core::{
    insight::*, EmittedEvent, QueuedEffectExecution, QueuedEvent, Store, NAMESPACE_SEESAW,
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

const EVENT_CLAIM_SECONDS: i64 = 30;

fn emitted_event_created_at(parent_created_at: DateTime<Utc>) -> DateTime<Utc> {
    parent_created_at
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight should always be a valid UTC timestamp")
        .and_utc()
}

fn effect_retry_delay_seconds(attempts: i32) -> i64 {
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
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct StateRow {
    state: serde_json::Value,
    version: i32,
}

#[derive(FromRow)]
struct EffectRow {
    event_id: Uuid,
    effect_id: String,
    correlation_id: Uuid,
    event_type: String,
    event_payload: serde_json::Value,
    parent_event_id: Option<Uuid>,
    execute_at: DateTime<Utc>,
    timeout_seconds: i32,
    max_attempts: i32,
    priority: i32,
    attempts: i32,
}

#[derive(FromRow)]
struct ParentEventRow {
    hops: i32,
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
                event_id, parent_id, correlation_id, event_type, payload, hops, created_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(event.event_id)
        .bind(event.parent_id)
        .bind(event.correlation_id)
        .bind(event.event_type)
        .bind(event.payload)
        .bind(event.hops)
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
            RETURNING e.id, e.event_id, e.parent_id, e.correlation_id, e.event_type, e.payload, e.hops, e.created_at",
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

    async fn load_state<S>(&self, correlation_id: Uuid) -> Result<Option<(S, i32)>>
    where
        S: for<'de> Deserialize<'de> + Send,
    {
        let row: Option<StateRow> =
            sqlx::query_as("SELECT state, version FROM seesaw_state WHERE correlation_id = $1")
                .bind(correlation_id)
                .fetch_optional(&self.pool)
                .await?;

        match row {
            Some(r) => {
                let state: S = serde_json::from_value(r.state)?;
                Ok(Some((state, r.version)))
            }
            None => Ok(None),
        }
    }

    async fn save_state<S>(
        &self,
        correlation_id: Uuid,
        state: &S,
        expected_version: i32,
    ) -> Result<i32>
    where
        S: Serialize + Send + Sync,
    {
        let state_json = serde_json::to_value(state)?;
        let new_version = expected_version + 1;

        let result = sqlx::query(
            "INSERT INTO seesaw_state (correlation_id, state, version, updated_at)
             VALUES ($1, $2, $3, NOW())
             ON CONFLICT (correlation_id) DO UPDATE
             SET state = $2,
                 version = $3,
                 updated_at = NOW()
             WHERE seesaw_state.version = $4",
        )
        .bind(correlation_id)
        .bind(&state_json)
        .bind(new_version)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            anyhow::bail!("Version conflict: state was modified concurrently");
        }

        Ok(new_version)
    }

    async fn insert_effect_intent(
        &self,
        event_id: Uuid,
        effect_id: String,
        correlation_id: Uuid,
        event_type: String,
        event_payload: serde_json::Value,
        parent_event_id: Option<Uuid>,
        execute_at: DateTime<Utc>,
        timeout_seconds: i32,
        max_attempts: i32,
        priority: i32,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO seesaw_effect_executions (
                event_id, effect_id, correlation_id, status,
                event_type, event_payload, parent_event_id,
                execute_at, timeout_seconds, max_attempts, priority
             )
             VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(event_id)
        .bind(effect_id)
        .bind(correlation_id)
        .bind(event_type)
        .bind(event_payload)
        .bind(parent_event_id)
        .bind(execute_at)
        .bind(timeout_seconds)
        .bind(max_attempts)
        .bind(priority)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn poll_next_effect(&self) -> Result<Option<QueuedEffectExecution>> {
        let row: Option<EffectRow> = sqlx::query_as(
            "WITH next_effect AS (
                SELECT event_id, effect_id
                FROM seesaw_effect_executions
                WHERE (
                    status = 'pending'
                    OR (status = 'failed' AND attempts < max_attempts)
                )
                  AND execute_at <= NOW()
                ORDER BY priority ASC, execute_at ASC, event_id ASC, effect_id ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE seesaw_effect_executions e
            SET status = 'executing',
                claimed_at = NOW(),
                last_attempted_at = NOW(),
                attempts = e.attempts + 1
            FROM next_effect
            WHERE e.event_id = next_effect.event_id
              AND e.effect_id = next_effect.effect_id
            RETURNING
                e.event_id, e.effect_id, e.correlation_id, e.event_type, e.event_payload, e.parent_event_id,
                e.execute_at, e.timeout_seconds, e.max_attempts, e.priority, e.attempts",
        )
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            Ok(Some(QueuedEffectExecution {
                event_id: r.event_id,
                effect_id: r.effect_id,
                correlation_id: r.correlation_id,
                event_type: r.event_type,
                event_payload: r.event_payload,
                parent_event_id: r.parent_event_id,
                execute_at: r.execute_at,
                timeout_seconds: r.timeout_seconds,
                max_attempts: r.max_attempts,
                priority: r.priority,
                attempts: r.attempts,
            }))
        } else {
            Ok(None)
        }
    }

    async fn complete_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        result: serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE seesaw_effect_executions
             SET status = 'completed',
                 result = $3,
                 completed_at = NOW()
             WHERE event_id = $1 AND effect_id = $2",
        )
        .bind(event_id)
        .bind(effect_id)
        .bind(result)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn complete_effect_with_events(
        &self,
        event_id: Uuid,
        effect_id: String,
        result: serde_json::Value,
        emitted_events: Vec<EmittedEvent>,
    ) -> Result<()> {
        // Get correlation_id and hops for emitted events
        let effect: EffectRow = sqlx::query_as(
            "SELECT event_id, effect_id, correlation_id, event_type, event_payload, parent_event_id,
                    execute_at, timeout_seconds, max_attempts, priority, attempts
             FROM seesaw_effect_executions
             WHERE event_id = $1 AND effect_id = $2",
        )
        .bind(event_id)
        .bind(&effect_id)
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
        for emitted in emitted_events {
            // Generate deterministic event_id from hash(parent_event_id, effect_id, event_type)
            let deterministic_id = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!("{}-{}-{}", event_id, effect_id, emitted.event_type).as_bytes(),
            );

            // Deterministic timestamp keeps retries idempotent while staying in
            // the same partition day as the parent event.
            let deterministic_timestamp = emitted_event_created_at(parent.created_at);

            // Insert event (idempotent via ON CONFLICT on (event_id, created_at))
            sqlx::query(
                "INSERT INTO seesaw_events (
                    event_id, parent_id, correlation_id, event_type, payload, hops, created_at
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (event_id, created_at) DO NOTHING",
            )
            .bind(deterministic_id)
            .bind(Some(event_id))
            .bind(effect.correlation_id)
            .bind(&emitted.event_type)
            .bind(emitted.payload)
            .bind(parent.hops + 1)
            .bind(deterministic_timestamp)
            .execute(&mut *tx)
            .await?;
        }

        // Mark effect as completed (same transaction)
        sqlx::query(
            "UPDATE seesaw_effect_executions
             SET status = 'completed',
                 result = $3,
                 completed_at = NOW()
             WHERE event_id = $1 AND effect_id = $2",
        )
        .bind(event_id)
        .bind(effect_id)
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
        effect_id: String,
        error: String,
        attempts: i32,
    ) -> Result<()> {
        let retry_at = Utc::now() + Duration::seconds(effect_retry_delay_seconds(attempts));
        sqlx::query(
            "UPDATE seesaw_effect_executions
             SET status = 'failed',
                 error = $3,
                 execute_at = $5,
                 claimed_at = NULL
             WHERE event_id = $1 AND effect_id = $2 AND attempts >= $4",
        )
        .bind(event_id)
        .bind(effect_id)
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
        effect_id: String,
        error: String,
        reason: String,
        attempts: i32,
    ) -> Result<()> {
        // Get effect details for DLQ
        let effect: EffectRow = sqlx::query_as(
            "SELECT event_id, effect_id, correlation_id, event_type, event_payload, parent_event_id,
                    execute_at, timeout_seconds, max_attempts, priority, attempts
             FROM seesaw_effect_executions
             WHERE event_id = $1 AND effect_id = $2",
        )
        .bind(event_id)
        .bind(&effect_id)
        .fetch_one(&self.pool)
        .await?;

        // Insert into DLQ
        sqlx::query(
            "INSERT INTO seesaw_dlq (
                event_id, effect_id, correlation_id, error, event_type, event_payload, reason, attempts
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(event_id)
        .bind(&effect_id)
        .bind(effect.correlation_id)
        .bind(error)
        .bind(effect.event_type)
        .bind(effect.event_payload)
        .bind(reason)
        .bind(attempts)
        .execute(&self.pool)
        .await?;

        // Delete from executions table
        sqlx::query("DELETE FROM seesaw_effect_executions WHERE event_id = $1 AND effect_id = $2")
            .bind(event_id)
            .bind(effect_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn subscribe_workflow_events(
        &self,
        correlation_id: Uuid,
    ) -> Result<Box<dyn futures::Stream<Item = seesaw_core::WorkflowEvent> + Send + Unpin>> {
        use futures::stream::StreamExt;
        use sqlx::postgres::PgListener;

        let channel = format!("seesaw_workflow_{}", correlation_id);

        // Create a new listener connection
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen(&channel).await?;

        let pool = self.pool.clone();

        // Convert listener into a stream of WorkflowEvent
        let stream = listener.into_stream().filter_map(move |result| {
            let pool = pool.clone();
            Box::pin(async move {
                match result {
                    Ok(notification) => {
                        // Parse notification metadata (no payload due to 8000-byte pg_notify limit)
                        #[derive(serde::Deserialize)]
                        struct NotificationMeta {
                            event_id: Uuid,
                            correlation_id: Uuid,
                            event_type: String,
                        }

                        let meta = serde_json::from_str::<NotificationMeta>(notification.payload()).ok()?;

                        // Fetch full event from database
                        sqlx::query_as::<_, (Uuid, Uuid, String, serde_json::Value)>(
                            "SELECT event_id, correlation_id, event_type, payload
                             FROM seesaw_events
                             WHERE event_id = $1"
                        )
                        .bind(meta.event_id)
                        .fetch_optional(&pool)
                        .await
                        .ok()?
                        .map(|(event_id, correlation_id, event_type, payload)| {
                            seesaw_core::WorkflowEvent {
                                event_id,
                                correlation_id,
                                event_type,
                                payload,
                            }
                        })
                    }
                    Err(_) => None,
                }
            })
        });

        Ok(Box::new(stream))
    }

    async fn get_workflow_status(
        &self,
        correlation_id: Uuid,
    ) -> Result<seesaw_core::WorkflowStatus> {
        let state = sqlx::query_as::<_, (serde_json::Value,)>(
            "SELECT state FROM seesaw_state WHERE correlation_id = $1",
        )
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|r| r.0);

        let pending_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_effect_executions
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
            state,
            pending_effects,
            is_settled: pending_effects == 0,
            last_event,
        })
    }
}

#[derive(FromRow)]
struct StreamRow {
    seq: i64,
    stream_type: String,
    correlation_id: Uuid,
    event_id: Option<Uuid>,
    effect_event_id: Option<Uuid>,
    effect_id: Option<String>,
    status: Option<String>,
    error: Option<String>,
    payload: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
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
                                    effect_id, status, error, payload, created_at
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
            "SELECT id, event_id, parent_id, correlation_id, event_type, payload, hops, created_at
             FROM seesaw_events
             WHERE correlation_id = $1
             ORDER BY created_at ASC",
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        // Get all effects for this correlation
        let effects = sqlx::query_as::<_, EffectTreeRow>(
            "SELECT event_id, effect_id, status, result, error, attempts, created_at
             FROM seesaw_effect_executions
             WHERE correlation_id = $1
             ORDER BY created_at ASC",
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        // Build tree structure
        let roots = build_event_tree(&events, &effects, None);

        // Get state
        let state = sqlx::query_as::<_, (serde_json::Value,)>(
            "SELECT state FROM seesaw_state WHERE correlation_id = $1",
        )
        .bind(correlation_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|r| r.0);

        Ok(WorkflowTree {
            correlation_id,
            roots,
            state,
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
            "SELECT COUNT(*) FROM seesaw_effect_executions
             WHERE status IN ('pending', 'executing')",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        let completed_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_effect_executions WHERE status = 'completed'",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        let failed_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_effect_executions WHERE status = 'failed'",
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
                        effect_id, status, error, payload, created_at
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
                        effect_id, status, error, payload, created_at
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
}

#[derive(FromRow)]
struct EffectTreeRow {
    event_id: Uuid,
    effect_id: String,
    status: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
    attempts: i32,
    created_at: DateTime<Utc>,
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
        effect_id: row.effect_id,
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
) -> Vec<EventNode> {
    events
        .iter()
        .filter(|e| e.parent_id == parent_id)
        .map(|event| {
            // Get effects for this event
            let event_effects: Vec<EffectNode> = effects
                .iter()
                .filter(|eff| eff.event_id == event.event_id)
                .map(|eff| EffectNode {
                    effect_id: eff.effect_id.clone(),
                    event_id: eff.event_id,
                    status: eff.status.clone(),
                    result: eff.result.clone(),
                    error: eff.error.clone(),
                    attempts: eff.attempts,
                    created_at: eff.created_at,
                })
                .collect();

            // Recursively build children
            let children = build_event_tree(events, effects, Some(event.event_id));

            EventNode {
                event_id: event.event_id,
                event_type: event.event_type.clone(),
                payload: event.payload.clone(),
                created_at: event.created_at,
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
    fn effect_retry_delay_seconds_uses_exponential_backoff() {
        assert_eq!(effect_retry_delay_seconds(1), 1);
        assert_eq!(effect_retry_delay_seconds(2), 2);
        assert_eq!(effect_retry_delay_seconds(3), 4);
        assert_eq!(effect_retry_delay_seconds(4), 8);
    }

    #[test]
    fn effect_retry_delay_seconds_is_capped() {
        assert_eq!(effect_retry_delay_seconds(9), 256);
        assert_eq!(effect_retry_delay_seconds(50), 256);
    }
}
