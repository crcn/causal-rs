//! Postgres-backed [`EventIdRegistry`](causal::EventIdRegistry).
//!
//! The authoritative global `event_id` index (A2) that lets the Kurrent
//! backend recognize a redelivery past its tail-window scan. Postgres'
//! `PRIMARY KEY` on `event_id` gives absolute, unbounded dedup; `register`
//! is first-write-wins via `ON CONFLICT DO NOTHING`. Schema is
//! [`EVENT_ID_REGISTRY_SCHEMA_SQL`] (also
//! `migrations/20260702_causal_event_ids.sql`).

/// Registry schema (`CREATE TABLE IF NOT EXISTS`). Exported for migration
/// pipelines; also run by [`PgEventIdRegistry::ensure_schema`]. Kept in sync
/// with `migrations/20260702_causal_event_ids.sql` and `docs/schema.sql`.
pub const EVENT_ID_REGISTRY_SCHEMA_SQL: &str =
    include_str!("sql/event_id_registry_schema.sql");

#[cfg(feature = "postgres")]
mod pg {
    use anyhow::Result;
    use async_trait::async_trait;
    use sqlx::PgPool;
    use uuid::Uuid;

    use causal::event_id_registry::{EventIdEntry, EventIdRegistry};
    use causal::types::{LogCursor, StreamRevision};

    /// Postgres-backed [`EventIdRegistry`]. `new` never runs DDL — apply
    /// [`EVENT_ID_REGISTRY_SCHEMA_SQL`](super::EVENT_ID_REGISTRY_SCHEMA_SQL)
    /// via a migration pipeline or [`ensure_schema`](Self::ensure_schema).
    pub struct PgEventIdRegistry {
        pool: PgPool,
    }

    impl PgEventIdRegistry {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        /// Idempotently create the registry table. Safe on every boot.
        pub async fn ensure_schema(pool: &PgPool) -> Result<()> {
            sqlx::raw_sql(super::EVENT_ID_REGISTRY_SCHEMA_SQL).execute(pool).await?;
            Ok(())
        }

        pub async fn new_with_ensure_schema(pool: PgPool) -> Result<Self> {
            Self::ensure_schema(&pool).await?;
            Ok(Self::new(pool))
        }
    }

    #[async_trait]
    impl EventIdRegistry for PgEventIdRegistry {
        async fn lookup(&self, event_ids: &[Uuid]) -> Result<Vec<Option<EventIdEntry>>> {
            if event_ids.is_empty() {
                return Ok(Vec::new());
            }
            // One round-trip: fetch every present id, then align to input order.
            let rows: Vec<(Uuid, i64, i64)> = sqlx::query_as(
                "SELECT event_id, stream_position, stream_revision \
                 FROM causal_event_ids WHERE event_id = ANY($1)",
            )
            .bind(event_ids)
            .fetch_all(&self.pool)
            .await?;
            let found: std::collections::HashMap<Uuid, EventIdEntry> = rows
                .into_iter()
                .map(|(id, pos, rev)| {
                    (
                        id,
                        EventIdEntry {
                            event_id: id,
                            stream_position: LogCursor::from_raw(pos as u64),
                            stream_revision: StreamRevision::from_raw(rev as u64),
                        },
                    )
                })
                .collect();
            Ok(event_ids.iter().map(|id| found.get(id).copied()).collect())
        }

        async fn register(&self, entries: &[EventIdEntry]) -> Result<()> {
            if entries.is_empty() {
                return Ok(());
            }
            // Unnest a single multi-row INSERT; first-write-wins per id.
            let ids: Vec<Uuid> = entries.iter().map(|e| e.event_id).collect();
            let positions: Vec<i64> =
                entries.iter().map(|e| e.stream_position.raw() as i64).collect();
            let revisions: Vec<i64> =
                entries.iter().map(|e| e.stream_revision.raw() as i64).collect();
            sqlx::query(
                "INSERT INTO causal_event_ids (event_id, stream_position, stream_revision) \
                 SELECT * FROM UNNEST($1::uuid[], $2::bigint[], $3::bigint[]) \
                 ON CONFLICT (event_id) DO NOTHING",
            )
            .bind(&ids)
            .bind(&positions)
            .bind(&revisions)
            .execute(&self.pool)
            .await?;
            Ok(())
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgEventIdRegistry;
