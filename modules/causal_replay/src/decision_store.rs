//! Postgres-backed [`DecisionStore`](causal::DecisionStore).
//!
//! Persists one durable decision per `(consumer, trigger_event_id)` — the
//! full output batch a reaction produced — so redelivery replays the record
//! instead of re-running the reactor body. The schema is
//! [`DECISION_SCHEMA_SQL`] (also shipped as
//! `migrations/20260702_causal_decisions.sql`).
//!
//! ## First-write-wins atomicity
//!
//! `seal()` mirrors [`PgEffectStore`](crate::PgEffectStore): a CTE makes the
//! INSERT and the canonical-row fetch a single round-trip. Either the
//! INSERT wins (`RETURNING` yields our row) or `ON CONFLICT DO NOTHING`
//! fires (the `UNION ALL SELECT` yields the winner's). Under two racing
//! *first* writes at `READ COMMITTED`, the losing side can momentarily see
//! **zero** rows (the winner's insert is not yet visible and our own did
//! nothing) — `seal()` bounded-retries the SELECT until the committed
//! winner is visible, so it never returns a spurious row-not-found.
//!
//! Output serialization (durable envelope projection + ` `
//! sanitization) is owned by [`DecisionRecord`](causal::DecisionRecord), so
//! this backend and the in-memory reference round-trip identically.

/// Decision-records schema (`CREATE TABLE / INDEX IF NOT EXISTS`). Exported
/// so consumers with a migration pipeline can apply it through their own
/// tooling; also run by [`PgDecisionStore::ensure_schema`]. Kept in sync
/// with `migrations/20260702_causal_decisions.sql` and `docs/schema.sql`.
pub const DECISION_SCHEMA_SQL: &str = include_str!("sql/decision_schema.sql");

#[cfg(feature = "postgres")]
mod pg {
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use causal::{DecisionRecord, DecisionStore};

    /// Postgres-backed [`DecisionStore`].
    ///
    /// Construct with a `PgPool` wired to a database where the decisions
    /// schema has been applied — via a migration pipeline
    /// ([`DECISION_SCHEMA_SQL`](super::DECISION_SCHEMA_SQL)) or
    /// [`ensure_schema`](Self::ensure_schema). `new` never runs DDL.
    pub struct PgDecisionStore {
        pool: PgPool,
    }

    impl PgDecisionStore {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        /// Idempotently create the decisions table and its index (see
        /// [`DECISION_SCHEMA_SQL`](super::DECISION_SCHEMA_SQL)). Safe to call
        /// on every boot. Not run implicitly by [`new`](Self::new) — apps
        /// with a migration pipeline should apply the DDL there.
        pub async fn ensure_schema(pool: &PgPool) -> Result<()> {
            sqlx::raw_sql(super::DECISION_SCHEMA_SQL).execute(pool).await?;
            Ok(())
        }

        /// [`ensure_schema`](Self::ensure_schema) then [`new`](Self::new).
        pub async fn new_with_ensure_schema(pool: PgPool) -> Result<Self> {
            Self::ensure_schema(&pool).await?;
            Ok(Self::new(pool))
        }

        async fn fetch(
            &self,
            consumer: &str,
            trigger_event_id: Uuid,
        ) -> Result<Option<(serde_json::Value, DateTime<Utc>)>> {
            let row: Option<(serde_json::Value, DateTime<Utc>)> = sqlx::query_as(
                "SELECT outputs, sealed_at FROM causal_decisions \
                 WHERE consumer = $1 AND trigger_event_id = $2",
            )
            .bind(consumer)
            .bind(trigger_event_id)
            .fetch_optional(&self.pool)
            .await?;
            Ok(row)
        }
    }

    #[async_trait]
    impl DecisionStore for PgDecisionStore {
        async fn seal(&self, rec: DecisionRecord) -> Result<DecisionRecord> {
            let outputs = rec.outputs_to_json()?;

            // Atomic first-write-wins via CTE: INSERT wins → RETURNING our
            // row; conflict → UNION ALL SELECT the winner's. `sealed_at` is
            // read back so the returned record reflects the canonical row.
            let row: Option<(serde_json::Value, DateTime<Utc>)> = sqlx::query_as(
                "WITH ins AS ( \
                     INSERT INTO causal_decisions \
                         (consumer, trigger_event_id, outputs, sealed_at) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (consumer, trigger_event_id) DO NOTHING \
                     RETURNING outputs, sealed_at \
                 ) \
                 SELECT outputs, sealed_at FROM ins \
                 UNION ALL \
                 SELECT outputs, sealed_at FROM causal_decisions \
                 WHERE consumer = $1 AND trigger_event_id = $2 \
                 LIMIT 1",
            )
            .bind(&rec.consumer)
            .bind(rec.trigger_event_id)
            .bind(&outputs)
            .bind(rec.sealed_at)
            .fetch_optional(&self.pool)
            .await?;

            // Zero-row race (A4b): our insert did nothing and the winner's
            // row is not yet visible. Retry the plain SELECT until the
            // committed winner appears.
            let (canonical_outputs, sealed_at) = match row {
                Some(r) => r,
                None => {
                    let mut found = None;
                    for _ in 0..50 {
                        if let Some(r) =
                            self.fetch(&rec.consumer, rec.trigger_event_id).await?
                        {
                            found = Some(r);
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                    found.ok_or_else(|| {
                        anyhow::anyhow!(
                            "decision seal returned no canonical row for \
                             (consumer={}, trigger={}) after retry",
                            rec.consumer,
                            rec.trigger_event_id,
                        )
                    })?
                }
            };

            Ok(DecisionRecord {
                consumer: rec.consumer,
                trigger_event_id: rec.trigger_event_id,
                outputs: DecisionRecord::outputs_from_json(canonical_outputs)?,
                sealed_at,
            })
        }

        async fn get(
            &self,
            consumer: &str,
            trigger_event_id: Uuid,
        ) -> Result<Option<DecisionRecord>> {
            match self.fetch(consumer, trigger_event_id).await? {
                Some((outputs, sealed_at)) => Ok(Some(DecisionRecord {
                    consumer: consumer.to_string(),
                    trigger_event_id,
                    outputs: DecisionRecord::outputs_from_json(outputs)?,
                    sealed_at,
                })),
                None => Ok(None),
            }
        }

        async fn remove(&self, consumer: &str, trigger_event_id: Uuid) -> Result<()> {
            sqlx::query(
                "DELETE FROM causal_decisions \
                 WHERE consumer = $1 AND trigger_event_id = $2",
            )
            .bind(consumer)
            .bind(trigger_event_id)
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        /// Age-driven retention GC (A1) — never keyed to the ack-floor, which
        /// carries no fencing token and would let a zombie holder re-seal a
        /// GC'd decision. Uses the `sealed_at` index.
        async fn remove_sealed_before(&self, cutoff: DateTime<Utc>) -> Result<u64> {
            let res = sqlx::query("DELETE FROM causal_decisions WHERE sealed_at < $1")
                .bind(cutoff)
                .execute(&self.pool)
                .await?;
            Ok(res.rows_affected())
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgDecisionStore;
