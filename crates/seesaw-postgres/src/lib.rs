use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use seesaw_core::{QueuedEffectExecution, QueuedEvent, Store};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// PostgreSQL implementation of Store trait
///
/// Features:
/// - Per-saga FIFO ordering with advisory locks
/// - Idempotency via UNIQUE(event_id)
/// - Optimistic locking for state (version column)
/// - Priority-based effect execution
/// - DLQ for permanently failed effects
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

#[async_trait]
impl Store for PostgresStore {
    // =========================================================================
    // Queue Operations
    // =========================================================================

    async fn publish(&self, event: QueuedEvent) -> Result<()> {
        sqlx::query!(
            "INSERT INTO seesaw_events (
                event_id, parent_id, saga_id, event_type, payload, hops, created_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (event_id) DO NOTHING",
            event.event_id,
            event.parent_id,
            event.saga_id,
            event.event_type,
            event.payload,
            event.hops,
            event.created_at
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        // Per-saga FIFO with advisory locks
        // pg_advisory_xact_lock ensures only one worker processes events for a saga at a time
        let row = sqlx::query!(
            r#"
            SELECT
                pg_advisory_xact_lock(hashtext(saga_id::text)),
                id, event_id, parent_id, saga_id, event_type, payload, hops, created_at
            FROM seesaw_events
            WHERE processed_at IS NULL
              AND (locked_until IS NULL OR locked_until < NOW())
            ORDER BY created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#
        )
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(QueuedEvent {
                id: r.id,
                event_id: r.event_id,
                parent_id: r.parent_id,
                saga_id: r.saga_id,
                event_type: r.event_type,
                payload: r.payload,
                hops: r.hops,
                created_at: r.created_at,
            })),
            None => Ok(None),
        }
    }

    async fn ack(&self, id: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE seesaw_events SET processed_at = NOW() WHERE id = $1",
            id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn nack(&self, id: i64, retry_after_secs: u64) -> Result<()> {
        let locked_until = Utc::now() + Duration::seconds(retry_after_secs as i64);

        sqlx::query!(
            "UPDATE seesaw_events
             SET retry_count = retry_count + 1,
                 locked_until = $2
             WHERE id = $1",
            id,
            locked_until
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // =========================================================================
    // State Operations
    // =========================================================================

    async fn load_state<S>(&self, saga_id: Uuid) -> Result<Option<(S, i32)>>
    where
        S: for<'de> Deserialize<'de> + Send,
    {
        let row = sqlx::query!(
            "SELECT state, version FROM seesaw_state WHERE saga_id = $1",
            saga_id
        )
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

        // Optimistic locking: only update if version matches
        let result = sqlx::query!(
            "INSERT INTO seesaw_state (saga_id, state, version, updated_at)
             VALUES ($1, $2, $3, NOW())
             ON CONFLICT (saga_id) DO UPDATE
             SET state = $2,
                 version = $3,
                 updated_at = NOW()
             WHERE seesaw_state.version = $4",
            saga_id,
            state_json,
            new_version,
            expected_version
        )
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            anyhow::bail!("Version conflict: state was modified concurrently");
        }

        Ok(new_version)
    }

    // =========================================================================
    // Effect Execution Operations
    // =========================================================================

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
        sqlx::query!(
            "INSERT INTO seesaw_effect_executions (
                event_id, effect_id, saga_id, status,
                event_type, event_payload, parent_event_id,
                execute_at, timeout_seconds, max_attempts, priority
             )
             VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $10)",
            event_id,
            effect_id,
            saga_id,
            event_type,
            event_payload,
            parent_event_id,
            execute_at,
            timeout_seconds,
            max_attempts,
            priority
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn poll_next_effect(&self) -> Result<Option<QueuedEffectExecution>> {
        // Priority-based polling (lower number = higher priority)
        let row = sqlx::query!(
            r#"
            SELECT
                event_id, effect_id, saga_id, event_type, event_payload, parent_event_id,
                execute_at, timeout_seconds, max_attempts, priority, attempts
            FROM seesaw_effect_executions
            WHERE status = 'pending'
              AND execute_at <= NOW()
            ORDER BY priority DESC, execute_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#
        )
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => {
                // Mark as executing
                sqlx::query!(
                    "UPDATE seesaw_effect_executions
                     SET status = 'executing',
                         claimed_at = NOW(),
                         last_attempted_at = NOW(),
                         attempts = attempts + 1
                     WHERE event_id = $1 AND effect_id = $2",
                    r.event_id,
                    r.effect_id
                )
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
                    attempts: r.attempts,
                }))
            }
            None => Ok(None),
        }
    }

    async fn complete_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        result: serde_json::Value,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE seesaw_effect_executions
             SET status = 'completed',
                 result = $3,
                 completed_at = NOW()
             WHERE event_id = $1 AND effect_id = $2",
            event_id,
            effect_id,
            result
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn fail_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        error: String,
        attempts: i32,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE seesaw_effect_executions
             SET status = 'failed',
                 error = $3
             WHERE event_id = $1 AND effect_id = $2 AND attempts >= $4",
            event_id,
            effect_id,
            error,
            attempts
        )
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
        let effect = sqlx::query!(
            "SELECT saga_id, event_type, event_payload FROM seesaw_effect_executions
             WHERE event_id = $1 AND effect_id = $2",
            event_id,
            effect_id
        )
        .fetch_one(&self.pool)
        .await?;

        // Insert into DLQ
        sqlx::query!(
            "INSERT INTO seesaw_dlq (
                event_id, effect_id, saga_id, error, event_type, event_payload, reason, attempts
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            event_id,
            effect_id,
            effect.saga_id,
            error,
            effect.event_type,
            effect.event_payload,
            reason,
            attempts
        )
        .execute(&self.pool)
        .await?;

        // Delete from executions table
        sqlx::query!(
            "DELETE FROM seesaw_effect_executions WHERE event_id = $1 AND effect_id = $2",
            event_id,
            effect_id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
