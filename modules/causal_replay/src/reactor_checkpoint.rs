//! Postgres-backed `CheckpointStore` + `ReactorCheckpoint` implementation.
//!
//! Reactors append their outputs directly to the log and advance their
//! cursor (at-least-once + idempotent) — there's no outbox. This type
//! provides per-consumer checkpoint storage (`causal_checkpoints`) plus
//! the failure store retry-attempt counters used to enforce a reactor's retry budget.

#[cfg(feature = "postgres")]
mod pg {
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use dashmap::DashMap;
    use sqlx::PgPool;
    use uuid::Uuid;

    use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
    use causal::types::LogCursor;

    /// Postgres-backed CheckpointStore + ReactorCheckpoint.
    ///
    /// Constructor takes a `PgPool` already wired to a database where
    /// migration 054 has been applied.
    ///
    /// Reactor-attempt counters are held in-memory in a DashMap (not
    /// persisted). Survives within one PgReactorCheckpoint instance;
    /// process restart resets counts. Acceptable for the failure store
    /// retry-budget use case — restarting a process effectively
    /// retries everything from scratch anyway, and re-incrementing
    /// the counter from zero hits the cap again before too long.
    /// A future schema migration can promote this to a real table
    /// if persistence becomes a hard requirement.
    pub struct PgReactorCheckpoint {
        pool: PgPool,
        reactor_attempts: Arc<DashMap<(String, Uuid), u32>>,
    }

    impl PgReactorCheckpoint {
        pub fn new(pool: PgPool) -> Self {
            Self {
                pool,
                reactor_attempts: Arc::new(DashMap::new()),
            }
        }
    }

    #[async_trait]
    impl CheckpointStore for PgReactorCheckpoint {
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
            // ABSOLUTE upsert — installs `pos` verbatim, including backwards.
            // Used only by lifecycle wiring that legitimately moves a cursor
            // down (build-time seeding, the clamp heal). The per-event hot
            // path uses `advance` (monotonic) instead, so a lagging
            // concurrent writer cannot regress a cursor through this method.
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

        async fn advance(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
            // Atomic monotonic advance: GREATEST collapses two racing writers
            // (the no-leasor two-node case) so a lagging worker can never
            // regress a more-advanced cursor. A single SQL statement — no
            // read-modify-write window.
            sqlx::query(
                "INSERT INTO causal_checkpoints (consumer_id, position)
                 VALUES ($1, $2)
                 ON CONFLICT (consumer_id) DO UPDATE
                   SET position = GREATEST(causal_checkpoints.position, EXCLUDED.position),
                       updated_at = now()",
            )
            .bind(consumer_id)
            .bind(pos.raw() as i64)
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        async fn clamp_ahead_of(&self, tip: LogCursor) -> Result<u64> {
            // Clamp DOWN TO the tip — never to 0. KurrentDB is append-only, so
            // events <= tip are byte-identical to what each consumer already
            // processed; only > tip is gone. Resetting to 0 would force a full
            // replay over all history (a divergence storm); clamping resumes
            // each cursor at the tip and re-processes nothing.
            let r = sqlx::query(
                "UPDATE causal_checkpoints SET position = $1, updated_at = now() \
                 WHERE position > $1",
            )
            .bind(tip.raw() as i64)
            .execute(&self.pool)
            .await?;
            Ok(r.rows_affected())
        }

        async fn list_consumers(&self) -> Result<Vec<String>> {
            let rows: Vec<(String,)> =
                sqlx::query_as("SELECT consumer_id FROM causal_checkpoints")
                    .fetch_all(&self.pool)
                    .await?;
            Ok(rows.into_iter().map(|(c,)| c).collect())
        }
    }

    #[async_trait]
    impl ReactorCheckpoint for PgReactorCheckpoint {
        async fn record_reactor_attempt(
            &self,
            consumer_id: &str,
            trigger_id: Uuid,
        ) -> Result<u32> {
            let key = (consumer_id.to_string(), trigger_id);
            let mut entry = self.reactor_attempts.entry(key).or_insert(0);
            *entry += 1;
            Ok(*entry)
        }

        async fn clear_reactor_attempts(
            &self,
            consumer_id: &str,
            trigger_id: Uuid,
        ) -> Result<()> {
            self.reactor_attempts
                .remove(&(consumer_id.to_string(), trigger_id));
            Ok(())
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgReactorCheckpoint;
