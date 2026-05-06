//! Postgres-backed `EventLogBackend` implementation.
//!
//! Schema lives in rootsignal migration 054_causal_v03_backend_tables.sql;
//! see docs/plans/2026-05-06-causal-v03-phase-4e-postgres-backend-plan.md
//! for the design rationale.
//!
//! Contracts honored:
//!
//! - **C1 (append idempotency on event_id):** the `causal_log` table
//!   has `UNIQUE(event_id)`. Duplicate appends collapse via the
//!   `INSERT ... ON CONFLICT DO UPDATE SET event_id = ...` trick that
//!   makes `RETURNING position` work in both fresh and duplicate cases.
//!
//! - **C6 (aggregate OCC):** `append_to_stream` relies on the partial
//!   `UNIQUE(aggregate_type, aggregate_id, version)` index. A stale
//!   `expected` produces the next version that collides; the backend
//!   catches the unique violation, looks up the current version, and
//!   returns `Conflict { current_version }`.
//!
//! Position gaps are allowed (rolled-back transactions leave gaps in
//! BIGSERIAL). Cursor consumers compare with `position > cursor`,
//! which is correct over gaps.

#[cfg(feature = "postgres")]
mod pg {
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use causal::types::{
        AppendResult, LogCursor, NewEvent, PersistedEvent, StreamVersion,
    };
    use causal::EventLogBackend;

    /// Postgres-backed event log.
    ///
    /// Construct with a `PgPool` already wired to a database where
    /// migration 054 has been applied. The backend does NOT auto-create
    /// its tables — that's the migration runner's job.
    pub struct PgEventLogBackend {
        pool: PgPool,
    }

    impl PgEventLogBackend {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl EventLogBackend for PgEventLogBackend {
        async fn append(&self, event: NewEvent) -> Result<AppendResult> {
            // ON CONFLICT DO UPDATE SET event_id = excluded.event_id is a
            // no-op update (event_id is the conflict target, so this
            // assignment is identity). Necessary because plain
            // ON CONFLICT DO NOTHING returns no row, which would force a
            // follow-up SELECT for the existing position. The no-op
            // update lets RETURNING work in both fresh and duplicate
            // cases — one round trip either way.
            let metadata =
                serde_json::Value::Object(event.metadata.clone());
            let row = sqlx::query(
                "INSERT INTO causal_log
                    (event_id, parent_id, correlation_id, event_type,
                     payload, aggregate_type, aggregate_id, version,
                     metadata, created_at, persistent)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 ON CONFLICT (event_id) DO UPDATE
                    SET event_id = excluded.event_id
                 RETURNING position, version",
            )
            .bind(event.event_id)
            .bind(event.parent_id)
            .bind(event.correlation_id)
            .bind(&event.event_type)
            .bind(&event.payload)
            .bind(event.aggregate_type.as_deref())
            .bind(event.aggregate_id)
            .bind(event.version_for_storage())
            .bind(&metadata)
            .bind(event.created_at)
            .bind(event.persistent)
            .fetch_one(&self.pool)
            .await?;

            let position: i64 = row.try_get("position")?;
            let version: Option<i64> = row.try_get("version")?;
            Ok(AppendResult {
                position: LogCursor::from_raw(position as u64),
                version: version.map(|v| StreamVersion::from_raw(v as u64)),
            })
        }

        async fn append_to_stream(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
            expected: StreamVersion,
            event: NewEvent,
        ) -> Result<AppendResult> {
            let next_version = (expected.raw() + 1) as i64;
            let metadata =
                serde_json::Value::Object(event.metadata.clone());

            let result = sqlx::query(
                "INSERT INTO causal_log
                    (event_id, parent_id, correlation_id, event_type,
                     payload, aggregate_type, aggregate_id, version,
                     metadata, created_at, persistent)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 RETURNING position",
            )
            .bind(event.event_id)
            .bind(event.parent_id)
            .bind(event.correlation_id)
            .bind(&event.event_type)
            .bind(&event.payload)
            .bind(aggregate_type)
            .bind(aggregate_id)
            .bind(next_version)
            .bind(&metadata)
            .bind(event.created_at)
            .bind(event.persistent)
            .fetch_one(&self.pool)
            .await;

            match result {
                Ok(row) => {
                    let position: i64 = row.try_get("position")?;
                    Ok(AppendResult {
                        position: LogCursor::from_raw(position as u64),
                        version: Some(StreamVersion::from_raw(next_version as u64)),
                    })
                }
                Err(e) => {
                    // Detect unique violation on the stream index. The
                    // trait surface returns generic anyhow::Error here
                    // (the v0.3 design doc names a `Conflict` variant
                    // for the engine layer; backend signals the
                    // collision via error and the engine maps it).
                    if let Some(db_err) = e.as_database_error() {
                        if db_err
                            .constraint()
                            .map(|c| c == "idx_causal_log_stream")
                            .unwrap_or(false)
                        {
                            // Look up the current head version so the
                            // caller can retry with the right expected.
                            let current: i64 = sqlx::query_scalar(
                                "SELECT COALESCE(MAX(version), 0)
                                   FROM causal_log
                                  WHERE aggregate_type = $1
                                    AND aggregate_id = $2",
                            )
                            .bind(aggregate_type)
                            .bind(aggregate_id)
                            .fetch_one(&self.pool)
                            .await?;
                            return Err(anyhow::anyhow!(
                                "OCC conflict on aggregate {}:{} — expected version {}, current is {}",
                                aggregate_type,
                                aggregate_id,
                                expected.raw(),
                                current,
                            ));
                        }
                    }
                    Err(e.into())
                }
            }
        }

        async fn load_from(
            &self,
            after: LogCursor,
            limit: usize,
        ) -> Result<Vec<PersistedEvent>> {
            let rows = sqlx::query(
                "SELECT position, event_id, parent_id, correlation_id,
                        event_type, payload, aggregate_type, aggregate_id,
                        version, metadata, created_at, persistent
                   FROM causal_log
                  WHERE position > $1
                  ORDER BY position ASC
                  LIMIT $2",
            )
            .bind(after.raw() as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

            rows.into_iter().map(row_to_persisted).collect()
        }

        async fn load_stream(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
            after_version: Option<StreamVersion>,
        ) -> Result<Vec<PersistedEvent>> {
            let after = after_version.map(|v| v.raw() as i64).unwrap_or(0);
            let rows = sqlx::query(
                "SELECT position, event_id, parent_id, correlation_id,
                        event_type, payload, aggregate_type, aggregate_id,
                        version, metadata, created_at, persistent
                   FROM causal_log
                  WHERE aggregate_type = $1
                    AND aggregate_id = $2
                    AND version > $3
                  ORDER BY version ASC",
            )
            .bind(aggregate_type)
            .bind(aggregate_id)
            .bind(after)
            .fetch_all(&self.pool)
            .await?;

            rows.into_iter().map(row_to_persisted).collect()
        }

        async fn latest_position(&self) -> Result<LogCursor> {
            let position: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(position), 0) FROM causal_log",
            )
            .fetch_one(&self.pool)
            .await?;
            Ok(LogCursor::from_raw(position as u64))
        }
    }

    fn row_to_persisted(row: sqlx::postgres::PgRow) -> Result<PersistedEvent> {
        let metadata_value: serde_json::Value =
            row.try_get("metadata")?;
        let metadata = match metadata_value {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        let position: i64 = row.try_get("position")?;
        let version: Option<i64> = row.try_get("version")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;

        Ok(PersistedEvent {
            position: LogCursor::from_raw(position as u64),
            event_id: row.try_get("event_id")?,
            parent_id: row.try_get("parent_id")?,
            correlation_id: row.try_get("correlation_id")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
            aggregate_type: row.try_get("aggregate_type")?,
            aggregate_id: row.try_get("aggregate_id")?,
            version: version.map(|v| StreamVersion::from_raw(v as u64)),
            metadata,
            created_at,
            ephemeral: None,
            persistent: row.try_get("persistent")?,
        })
    }

    /// Helper trait for `NewEvent` to project its `version` field as the
    /// nullable storage-side BIGINT. NewEvent doesn't currently expose a
    /// `version` field directly (StreamVersion is set by `append_to_stream`,
    /// not on NewEvent), so we derive it from the aggregate-shape: events
    /// with `aggregate_type == None` have `version = NULL`. For
    /// `append_to_stream`, the caller supplies the version; this helper
    /// is only used by `append`.
    trait NewEventVersionExt {
        fn version_for_storage(&self) -> Option<i64>;
    }

    impl NewEventVersionExt for NewEvent {
        fn version_for_storage(&self) -> Option<i64> {
            // append() is for non-aggregate events; version is always None.
            // append_to_stream() bypasses this code path and binds version
            // explicitly. If a NewEvent constructed for append() somehow
            // has aggregate_type set, we still write NULL — append() is
            // not the OCC-protected path, and writing the version without
            // OCC would defeat the C6 contract.
            None
        }
    }

}

#[cfg(feature = "postgres")]
pub use pg::PgEventLogBackend;
