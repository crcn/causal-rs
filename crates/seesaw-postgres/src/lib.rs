// Simplified PostgresStore without compile-time checked queries
// Uses dynamic queries for easier testing

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use seesaw_core::{EmittedEvent, QueuedEffectExecution, QueuedEvent, Store, NAMESPACE_SEESAW};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

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
    saga_id: Uuid,
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
    saga_id: Uuid,
    event_type: String,
    event_payload: serde_json::Value,
    parent_event_id: Option<Uuid>,
    execute_at: DateTime<Utc>,
    timeout_seconds: i32,
    max_attempts: i32,
    priority: i32,
    attempts: i32,
}

#[async_trait]
impl Store for PostgresStore {
    async fn publish(&self, event: QueuedEvent) -> Result<()> {
        sqlx::query(
            "INSERT INTO seesaw_events (
                event_id, parent_id, saga_id, event_type, payload, hops, created_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (event_id, created_at) DO NOTHING"
        )
        .bind(event.event_id)
        .bind(event.parent_id)
        .bind(event.saga_id)
        .bind(event.event_type)
        .bind(event.payload)
        .bind(event.hops)
        .bind(event.created_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        let row: Option<EventRow> = sqlx::query_as(
            "SELECT
                pg_advisory_xact_lock(hashtext(saga_id::text)),
                id, event_id, parent_id, saga_id, event_type, payload, hops, created_at
            FROM seesaw_events
            WHERE processed_at IS NULL
              AND (locked_until IS NULL OR locked_until < NOW())
            ORDER BY created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED"
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| QueuedEvent {
            id: r.id,
            event_id: r.event_id,
            parent_id: r.parent_id,
            saga_id: r.saga_id,
            event_type: r.event_type,
            payload: r.payload,
            hops: r.hops,
            created_at: r.created_at,
        }))
    }

    async fn ack(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE seesaw_events SET processed_at = NOW() WHERE id = $1")
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
             WHERE id = $1"
        )
        .bind(id)
        .bind(locked_until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_state<S>(&self, saga_id: Uuid) -> Result<Option<(S, i32)>>
    where
        S: for<'de> Deserialize<'de> + Send,
    {
        let row: Option<StateRow> = sqlx::query_as(
            "SELECT state, version FROM seesaw_state WHERE saga_id = $1"
        )
        .bind(saga_id)
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

    async fn save_state<S>(&self, saga_id: Uuid, state: &S, expected_version: i32) -> Result<i32>
    where
        S: Serialize + Send + Sync,
    {
        let state_json = serde_json::to_value(state)?;
        let new_version = expected_version + 1;

        let result = sqlx::query(
            "INSERT INTO seesaw_state (saga_id, state, version, updated_at)
             VALUES ($1, $2, $3, NOW())
             ON CONFLICT (saga_id) DO UPDATE
             SET state = $2,
                 version = $3,
                 updated_at = NOW()
             WHERE seesaw_state.version = $4"
        )
        .bind(saga_id)
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
        saga_id: Uuid,
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
                event_id, effect_id, saga_id, status,
                event_type, event_payload, parent_event_id,
                execute_at, timeout_seconds, max_attempts, priority
             )
             VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $10)"
        )
        .bind(event_id)
        .bind(effect_id)
        .bind(saga_id)
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
            "SELECT
                event_id, effect_id, saga_id, event_type, event_payload, parent_event_id,
                execute_at, timeout_seconds, max_attempts, priority, attempts
            FROM seesaw_effect_executions
            WHERE status = 'pending'
              AND execute_at <= NOW()
            ORDER BY priority ASC, execute_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED"
        )
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            // Mark as executing
            sqlx::query(
                "UPDATE seesaw_effect_executions
                 SET status = 'executing',
                     claimed_at = NOW(),
                     last_attempted_at = NOW(),
                     attempts = attempts + 1
                 WHERE event_id = $1 AND effect_id = $2"
            )
            .bind(r.event_id)
            .bind(&r.effect_id)
            .execute(&self.pool)
            .await?;

            Ok(Some(QueuedEffectExecution {
                event_id: r.event_id,
                effect_id: r.effect_id,
                saga_id: r.saga_id,
                event_type: r.event_type,
                event_payload: r.event_payload,
                parent_event_id: r.parent_event_id,
                execute_at: r.execute_at,
                timeout_seconds: r.timeout_seconds,
                max_attempts: r.max_attempts,
                priority: r.priority,
                attempts: r.attempts + 1, // Already incremented in UPDATE
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
             WHERE event_id = $1 AND effect_id = $2"
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
        // Get saga_id and hops for emitted events
        let effect: EffectRow = sqlx::query_as(
            "SELECT event_id, effect_id, saga_id, event_type, event_payload, parent_event_id,
                    execute_at, timeout_seconds, max_attempts, priority, attempts
             FROM seesaw_effect_executions
             WHERE event_id = $1 AND effect_id = $2"
        )
        .bind(event_id)
        .bind(&effect_id)
        .fetch_one(&self.pool)
        .await?;

        // Get hops from parent event
        let parent_hops: i32 = sqlx::query_scalar(
            "SELECT hops FROM seesaw_events WHERE event_id = $1"
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

            // Use fixed timestamp for idempotency
            // This ensures (event_id, created_at) pair is deterministic across crash+retry
            // Fixed to start of 2026 to stay in current partition
            let deterministic_timestamp = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();

            // Insert event (idempotent via ON CONFLICT on (event_id, created_at))
            sqlx::query(
                "INSERT INTO seesaw_events (
                    event_id, parent_id, saga_id, event_type, payload, hops, created_at
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (event_id, created_at) DO NOTHING"
            )
            .bind(deterministic_id)
            .bind(Some(event_id))
            .bind(effect.saga_id)
            .bind(&emitted.event_type)
            .bind(emitted.payload)
            .bind(parent_hops + 1)
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
             WHERE event_id = $1 AND effect_id = $2"
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
        sqlx::query(
            "UPDATE seesaw_effect_executions
             SET status = 'failed',
                 error = $3
             WHERE event_id = $1 AND effect_id = $2 AND attempts >= $4"
        )
        .bind(event_id)
        .bind(effect_id)
        .bind(error)
        .bind(attempts)
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
            "SELECT event_id, effect_id, saga_id, event_type, event_payload, parent_event_id,
                    execute_at, timeout_seconds, max_attempts, priority, attempts
             FROM seesaw_effect_executions
             WHERE event_id = $1 AND effect_id = $2"
        )
        .bind(event_id)
        .bind(&effect_id)
        .fetch_one(&self.pool)
        .await?;

        // Insert into DLQ
        sqlx::query(
            "INSERT INTO seesaw_dlq (
                event_id, effect_id, saga_id, error, event_type, event_payload, reason, attempts
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
        )
        .bind(event_id)
        .bind(&effect_id)
        .bind(effect.saga_id)
        .bind(error)
        .bind(effect.event_type)
        .bind(effect.event_payload)
        .bind(reason)
        .bind(attempts)
        .execute(&self.pool)
        .await?;

        // Delete from executions table
        sqlx::query(
            "DELETE FROM seesaw_effect_executions WHERE event_id = $1 AND effect_id = $2"
        )
        .bind(event_id)
        .bind(effect_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn subscribe_saga_events(
        &self,
        saga_id: Uuid,
    ) -> Result<Box<dyn futures::Stream<Item = seesaw_core::SagaEvent> + Send + Unpin>> {
        use futures::stream::StreamExt;
        use sqlx::postgres::PgListener;

        let channel = format!("seesaw_saga_{}", saga_id);

        // Create a new listener connection
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen(&channel).await?;

        // Convert listener into a stream of SagaEvent
        let stream = listener.into_stream().filter_map(|result| {
            Box::pin(async move {
                match result {
                    Ok(notification) => {
                        // Parse the JSON payload from the notification
                        if let Ok(event) = serde_json::from_str::<seesaw_core::SagaEvent>(notification.payload()) {
                            Some(event)
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
}
