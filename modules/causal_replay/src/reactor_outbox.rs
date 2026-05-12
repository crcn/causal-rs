//! Postgres-backed `CheckpointStore` + `ReactorOutbox` implementation.
//!
//! Schema lives in rootsignal migration 054_causal_v03_backend_tables.sql.
//! See docs/plans/2026-05-06-causal-v03-phase-4e-postgres-backend-plan.md
//! for the design rationale.
//!
//! Honors the **C12 atomicity contract**: `commit_reactor_batch` writes
//! N outbox rows AND advances a cursor in one transaction. Partial
//! observable state is a contract violation, so this is the most
//! crash-sensitive code in the v0.3 backend.
//!
//! Cursor advance uses a CAS-style `WHERE position < EXCLUDED.position`
//! guard — never goes backwards. Critical for at-least-once delivery:
//! if a stale checkpoint write races with a newer one, the newer wins.

#[cfg(feature = "postgres")]
mod pg {
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use dashmap::DashMap;
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use causal::checkpoint_store::{
        CheckpointStore, InsertableOutboxRow, OutboxRow, ReactorOutbox,
    };
    use causal::types::LogCursor;

    /// Postgres-backed CheckpointStore + ReactorOutbox.
    ///
    /// Constructor takes a `PgPool` already wired to a database where
    /// migration 054 has been applied.
    ///
    /// Reactor-attempt counters are held in-memory in a DashMap (not
    /// persisted). Survives within one PgReactorOutbox instance;
    /// process restart resets counts. Acceptable for the DLQ
    /// retry-budget use case — restarting a process effectively
    /// retries everything from scratch anyway, and re-incrementing
    /// the counter from zero hits the cap again before too long.
    /// A future schema migration can promote this to a real table
    /// if persistence becomes a hard requirement.
    pub struct PgReactorOutbox {
        pool: PgPool,
        reactor_attempts: Arc<DashMap<(String, Uuid), u32>>,
    }

    impl PgReactorOutbox {
        pub fn new(pool: PgPool) -> Self {
            Self {
                pool,
                reactor_attempts: Arc::new(DashMap::new()),
            }
        }
    }

    #[async_trait]
    impl CheckpointStore for PgReactorOutbox {
        async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT position FROM causal_checkpoints WHERE consumer_id = $1",
            )
            .bind(consumer_id)
            .fetch_optional(&self.pool)
            .await?;
            Ok(row.map(|(pos,)| LogCursor::from_raw(pos as u64)))
        }

        async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
            // Plain set: unconditional upsert. The atomic
            // commit_reactor_batch path uses the CAS variant — see
            // commit_reactor_batch below — to prevent going backwards.
            // This entry point is for cases where the caller is
            // explicitly asserting "this is the new cursor regardless"
            // (e.g., reset_projection ops).
            sqlx::query(
                "INSERT INTO causal_checkpoints (consumer_id, position)
                 VALUES ($1, $2)
                 ON CONFLICT (consumer_id) DO UPDATE
                   SET position = EXCLUDED.position,
                       updated_at = now()",
            )
            .bind(consumer_id)
            .bind(pos.raw() as i64)
            .execute(&self.pool)
            .await?;
            Ok(())
        }
    }

    #[async_trait]
    impl ReactorOutbox for PgReactorOutbox {
        async fn commit_reactor_batch(
            &self,
            rows: Vec<InsertableOutboxRow>,
            cursor: Option<(String, LogCursor)>,
        ) -> Result<()> {
            // C12: outbox rows + cursor advance MUST be one transaction.
            // Begin tx, insert rows with conflict-do-nothing dedup,
            // advance cursor with CAS guard, commit.
            let mut tx = self.pool.begin().await?;

            for row in &rows {
                sqlx::query(
                    "INSERT INTO causal_outbox
                        (reactor_id, source_event_id, output_index,
                         event_id, event_type, fact_payload, correlation_id)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (reactor_id, source_event_id, output_index)
                       DO NOTHING",
                )
                .bind(&row.reactor_id)
                .bind(row.source_event_id)
                .bind(row.output_index as i32)
                .bind(row.event_id)
                .bind(&row.event_type)
                .bind(&row.fact_payload)
                .bind(row.correlation_id)
                .execute(&mut *tx)
                .await?;
            }

            if let Some((consumer_id, pos)) = cursor {
                // CAS-style: only advance if the new position is greater
                // than what's stored. Prevents accidental rewind from a
                // stale concurrent writer.
                sqlx::query(
                    "INSERT INTO causal_checkpoints (consumer_id, position)
                     VALUES ($1, $2)
                     ON CONFLICT (consumer_id) DO UPDATE
                       SET position = EXCLUDED.position,
                           updated_at = now()
                       WHERE causal_checkpoints.position < EXCLUDED.position",
                )
                .bind(&consumer_id)
                .bind(pos.raw() as i64)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;
            Ok(())
        }

        async fn outbox_pending(&self, limit: usize) -> Result<Vec<OutboxRow>> {
            let rows = sqlx::query(
                "SELECT id, reactor_id, source_event_id, output_index,
                        event_id, event_type, fact_payload, correlation_id,
                        created_at
                   FROM causal_outbox
                   ORDER BY created_at ASC, id ASC
                   LIMIT $1",
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

            rows.into_iter().map(row_to_outbox).collect()
        }

        async fn outbox_delete(&self, id: i64) -> Result<()> {
            // Idempotent: DELETE on missing row is Ok per Postgres
            // semantics. The trait contract requires this — relay may
            // retry deletes after a partial crash.
            sqlx::query("DELETE FROM causal_outbox WHERE id = $1")
                .bind(id)
                .execute(&self.pool)
                .await?;
            Ok(())
        }

        async fn record_reactor_attempt(
            &self,
            consumer_id: &str,
            source_event_id: Uuid,
        ) -> Result<u32> {
            let key = (consumer_id.to_string(), source_event_id);
            let mut entry = self.reactor_attempts.entry(key).or_insert(0);
            *entry += 1;
            Ok(*entry)
        }

        async fn clear_reactor_attempts(
            &self,
            consumer_id: &str,
            source_event_id: Uuid,
        ) -> Result<()> {
            self.reactor_attempts
                .remove(&(consumer_id.to_string(), source_event_id));
            Ok(())
        }
    }

    fn row_to_outbox(row: sqlx::postgres::PgRow) -> Result<OutboxRow> {
        let id: i64 = row.try_get("id")?;
        let output_index: i32 = row.try_get("output_index")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        Ok(OutboxRow {
            id,
            reactor_id: row.try_get("reactor_id")?,
            source_event_id: row.try_get("source_event_id")?,
            output_index: output_index as u32,
            event_id: row.try_get("event_id")?,
            event_type: row.try_get("event_type")?,
            fact_payload: row.try_get("fact_payload")?,
            correlation_id: row.try_get("correlation_id")?,
            created_at,
        })
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgReactorOutbox;
