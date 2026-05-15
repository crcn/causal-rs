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
//!   `UNIQUE(aggregate_type, aggregate_id, revision)` index. A stale
//!   `expected` produces the next revision that collides; the backend
//!   catches the unique violation, looks up the current revision, and
//!   returns an `OCC conflict` error message.
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
        WriteResult, EventData, LogCursor, RecordedEvent, StreamRevision, StreamState,
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
        async fn append(&self, event: EventData) -> Result<WriteResult> {
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
                    (event_id, causation_id, correlation_id, event_type,
                     payload, aggregate_type, aggregate_id, revision,
                     metadata, created_at, persistent)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 ON CONFLICT (event_id) DO UPDATE
                    SET event_id = excluded.event_id
                 RETURNING position, revision",
            )
            .bind(event.event_id)
            .bind(event.causation_id)
            .bind(event.correlation_id)
            .bind(&event.event_type)
            .bind(&event.payload)
            .bind(event.aggregate_type.as_deref())
            .bind(event.aggregate_id)
            .bind(event.revision_for_storage())
            .bind(&metadata)
            .bind(event.created_at)
            .bind(event.persistent)
            .fetch_one(&self.pool)
            .await?;

            let position: i64 = row.try_get("position")?;
            let revision: Option<i64> = row.try_get("revision")?;
            Ok(WriteResult {
                position: LogCursor::from_raw(position as u64),
                revision: revision.map(|r| StreamRevision::from_raw(r as u64)),
            })
        }

        async fn append_to_stream(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
            expected: StreamState,
            event: EventData,
        ) -> Result<WriteResult> {
            // Compute the new event's revision (0-indexed).
            // For NoStream: 0 (the first event).
            // For StreamRevision(n): n + 1 (the event after that).
            // For StreamExists / Any: we have to read the current tail
            // first.
            let next_revision: i64 = match expected {
                StreamState::NoStream => 0,
                StreamState::StreamRevision(n) => (n + 1) as i64,
                StreamState::StreamExists | StreamState::Any => {
                    let current: Option<i64> = sqlx::query_scalar(
                        "SELECT MAX(revision)
                           FROM causal_log
                          WHERE aggregate_type = $1
                            AND aggregate_id = $2",
                    )
                    .bind(aggregate_type)
                    .bind(aggregate_id)
                    .fetch_one(&self.pool)
                    .await?;
                    match (expected, current) {
                        (StreamState::StreamExists, None) => {
                            return Err(anyhow::anyhow!(
                                "OCC conflict on aggregate {}:{} — \
                                 expected StreamExists, stream is empty",
                                aggregate_type, aggregate_id,
                            ));
                        }
                        (_, Some(c)) => c + 1,
                        (_, None) => 0,
                    }
                }
            };
            let metadata =
                serde_json::Value::Object(event.metadata.clone());

            let result = sqlx::query(
                "INSERT INTO causal_log
                    (event_id, causation_id, correlation_id, event_type,
                     payload, aggregate_type, aggregate_id, revision,
                     metadata, created_at, persistent)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 RETURNING position",
            )
            .bind(event.event_id)
            .bind(event.causation_id)
            .bind(event.correlation_id)
            .bind(&event.event_type)
            .bind(&event.payload)
            .bind(aggregate_type)
            .bind(aggregate_id)
            .bind(next_revision)
            .bind(&metadata)
            .bind(event.created_at)
            .bind(event.persistent)
            .fetch_one(&self.pool)
            .await;

            match result {
                Ok(row) => {
                    let position: i64 = row.try_get("position")?;
                    Ok(WriteResult {
                        position: LogCursor::from_raw(position as u64),
                        revision: Some(StreamRevision::from_raw(next_revision as u64)),
                    })
                }
                Err(e) => {
                    if let Some(db_err) = e.as_database_error() {
                        if db_err
                            .constraint()
                            .map(|c| c == "idx_causal_log_stream")
                            .unwrap_or(false)
                        {
                            // Look up the current head revision so the
                            // caller knows what to retry with.
                            let current: Option<i64> = sqlx::query_scalar(
                                "SELECT MAX(revision)
                                   FROM causal_log
                                  WHERE aggregate_type = $1
                                    AND aggregate_id = $2",
                            )
                            .bind(aggregate_type)
                            .bind(aggregate_id)
                            .fetch_one(&self.pool)
                            .await?;
                            return Err(anyhow::anyhow!(
                                "OCC conflict on aggregate {}:{} — expected {}, current revision is {:?}",
                                aggregate_type,
                                aggregate_id,
                                expected,
                                current,
                            ));
                        }
                    }
                    Err(e.into())
                }
            }
        }

        async fn read_all(
            &self,
            after: LogCursor,
            limit: usize,
        ) -> Result<Vec<RecordedEvent>> {
            let rows = sqlx::query(
                "SELECT position, event_id, causation_id, correlation_id,
                        event_type, payload, aggregate_type, aggregate_id,
                        revision, metadata, created_at, persistent
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

        async fn read_stream(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
            after: Option<StreamRevision>,
        ) -> Result<Vec<RecordedEvent>> {
            // 0-indexed `after`: caller asks for events with
            // revision > after. For `None`, return everything from
            // the stream. Use `-1` as the floor for the `None` case
            // so `revision > -1` matches `revision >= 0`.
            let after_val: i64 = after.map(|r| r.raw() as i64).unwrap_or(-1);
            let rows = sqlx::query(
                "SELECT position, event_id, causation_id, correlation_id,
                        event_type, payload, aggregate_type, aggregate_id,
                        revision, metadata, created_at, persistent
                   FROM causal_log
                  WHERE aggregate_type = $1
                    AND aggregate_id = $2
                    AND revision > $3
                  ORDER BY revision ASC",
            )
            .bind(aggregate_type)
            .bind(aggregate_id)
            .bind(after_val)
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

    fn row_to_persisted(row: sqlx::postgres::PgRow) -> Result<RecordedEvent> {
        let metadata_value: serde_json::Value =
            row.try_get("metadata")?;
        let metadata = match metadata_value {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        let position: i64 = row.try_get("position")?;
        let revision: Option<i64> = row.try_get("revision")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;

        Ok(RecordedEvent {
            position: LogCursor::from_raw(position as u64),
            event_id: row.try_get("event_id")?,
            causation_id: row.try_get("causation_id")?,
            correlation_id: row.try_get("correlation_id")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
            aggregate_type: row.try_get("aggregate_type")?,
            aggregate_id: row.try_get("aggregate_id")?,
            revision: revision.map(|r| StreamRevision::from_raw(r as u64)),
            metadata,
            created_at,
            ephemeral: None,
            persistent: row.try_get("persistent")?,
        })
    }

    /// Helper for `append()` (non-CAS path): the column is always
    /// NULL because `append()` is for non-aggregate events. The
    /// CAS path (`append_to_stream`) binds revision explicitly.
    trait EventDataRevisionExt {
        fn revision_for_storage(&self) -> Option<i64>;
    }

    impl EventDataRevisionExt for EventData {
        fn revision_for_storage(&self) -> Option<i64> {
            None
        }
    }

}

#[cfg(feature = "postgres")]
pub use pg::PgEventLogBackend;
