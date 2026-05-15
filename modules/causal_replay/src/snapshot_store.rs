//! Postgres-backed `SnapshotStore` implementation.
//!
//! Schema lives in rootsignal migrations 054 + 055. See
//! docs/plans/2026-05-06-causal-v03-phase-4e-postgres-backend-plan.md.
//!
//! Snapshots are keyed by `(aggregate_type, aggregate_id)`. The schema
//! uses a single `key` column encoding both as `"{type}:{id}"`; this
//! module owns the encoding so the schema stays string-agnostic.
//!
//! `load_snapshot` returns the latest snapshot for an aggregate by
//! `ORDER BY version DESC LIMIT 1`. Older snapshots remain in the
//! table; they're free per Phase 4 decisions.

#[cfg(feature = "postgres")]
mod pg {
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use causal::snapshot_store::SnapshotStore;
    use causal::types::{Snapshot, StreamRevision};

    /// Encode `(aggregate_type, aggregate_id)` into a single key.
    /// Using a colon separator that aggregate_type is not allowed to
    /// contain (per the v0.3 fact macro convention — colons separate
    /// the prefix from the variant in event_type, but aggregate_type
    /// is just the prefix).
    fn encode_key(aggregate_type: &str, aggregate_id: Uuid) -> String {
        format!("{}:{}", aggregate_type, aggregate_id)
    }

    pub struct PgSnapshotStore {
        pool: PgPool,
    }

    impl PgSnapshotStore {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl SnapshotStore for PgSnapshotStore {
        async fn load_snapshot(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
        ) -> Result<Option<Snapshot>> {
            let key = encode_key(aggregate_type, aggregate_id);
            let row = sqlx::query(
                "SELECT version, blob, created_at
                   FROM causal_snapshots
                  WHERE key = $1
                  ORDER BY version DESC
                  LIMIT 1",
            )
            .bind(&key)
            .fetch_optional(&self.pool)
            .await?;

            let Some(row) = row else { return Ok(None); };
            let revision: i64 = row.try_get("version")?;
            let blob: Vec<u8> = row.try_get("blob")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let state: serde_json::Value = serde_json::from_slice(&blob)?;

            Ok(Some(Snapshot {
                aggregate_type: aggregate_type.to_string(),
                aggregate_id,
                revision: StreamRevision::from_raw(revision as u64),
                state,
                created_at,
            }))
        }

        async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()> {
            let key = encode_key(&snapshot.aggregate_type, snapshot.aggregate_id);
            let blob = serde_json::to_vec(&snapshot.state)?;
            // ON CONFLICT (key, version) DO NOTHING — same key+version
            // saved twice is a no-op (idempotent retry of a partial
            // crash). The snapshot's state for a given (aggregate, version)
            // is deterministic — it's the fold of all events up to that
            // version — so duplicate writes are guaranteed-equal.
            sqlx::query(
                "INSERT INTO causal_snapshots (key, version, blob, created_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (key, version) DO NOTHING",
            )
            .bind(&key)
            .bind(snapshot.revision.raw() as i64)
            .bind(&blob)
            .bind(snapshot.created_at)
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        async fn delete_snapshot(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
        ) -> Result<()> {
            let key = encode_key(aggregate_type, aggregate_id);
            sqlx::query("DELETE FROM causal_snapshots WHERE key = $1")
                .bind(&key)
                .execute(&self.pool)
                .await?;
            Ok(())
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgSnapshotStore;
