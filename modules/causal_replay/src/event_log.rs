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
    use causal::event_log::ConflictError;
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
        async fn append_to_stream(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
            expected: StreamState,
            events: Vec<EventData>,
        ) -> Result<WriteResult> {
            let Some(last_event_id) = events.last().map(|e| e.event_id) else {
                anyhow::bail!("append_to_stream: events must be non-empty");
            };

            // One transaction so the whole batch lands atomically: a
            // partial multi-fact decision is never observable, and a
            // mid-batch failure rolls back cleanly.
            let mut tx = self.pool.begin().await?;

            // Revision of the FIRST event in the batch (0-indexed); the
            // rest follow at consecutive revisions.
            //   NoStream         → 0
            //   StreamRevision(n)→ n + 1
            //   StreamExists/Any → read the current tail first.
            let base_revision: i64 = match expected {
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
                    .fetch_one(&mut *tx)
                    .await?;
                    match (expected, current) {
                        (StreamState::StreamExists, None) => {
                            // Typed ConflictError so Engine::append can
                            // downcast + retry (not a bare string).
                            return Err(anyhow::Error::new(ConflictError {
                                expected,
                                current: None,
                            }));
                        }
                        (_, Some(c)) => c + 1,
                        (_, None) => 0,
                    }
                }
            };

            let mut result = WriteResult {
                position: LogCursor::ZERO,
                revision: StreamRevision::from_raw(0),
            };
            for (offset, event) in events.iter().enumerate() {
                let revision = base_revision + offset as i64;
                let metadata = serde_json::Value::Object(event.metadata.clone());
                let row = sqlx::query(
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
                .bind(revision)
                .bind(&metadata)
                .bind(event.created_at)
                .bind(event.persistent)
                .fetch_one(&mut *tx)
                .await;

                match row {
                    Ok(row) => {
                        let position: i64 = row.try_get("position")?;
                        result = WriteResult {
                            position: LogCursor::from_raw(position as u64),
                            revision: StreamRevision::from_raw(revision as u64),
                        };
                    }
                    Err(e) => {
                        let constraint =
                            e.as_database_error().and_then(|d| d.constraint());
                        // Idempotency (C1): a batch lands atomically, so a
                        // duplicate event_id means the whole batch is already
                        // persisted — return its WriteResult, never an error.
                        // Reactors rely on this for crash-redelivery safety
                        // (re-append after a crash before the cursor advances).
                        if constraint == Some("causal_log_event_id_key") {
                            drop(tx);
                            // Idempotent retry: the batch was already written, so
                            // its last event is present — return its result. If
                            // the last event is ABSENT, only part of the batch
                            // previously landed: a partial-overlap batch that
                            // violates the all-new-or-all-present precondition.
                            // Fail with a clear error rather than the opaque
                            // RowNotFound a `fetch_one` would raise.
                            let row = sqlx::query(
                                "SELECT position, revision FROM causal_log \
                                 WHERE event_id = $1",
                            )
                            .bind(last_event_id)
                            .fetch_optional(&self.pool)
                            .await?;
                            let Some(row) = row else {
                                return Err(anyhow::anyhow!(
                                    "append_to_stream on {}:{}: a batch event_id \
                                     already exists but the batch tail does not — \
                                     partial-overlap batch (event_ids must be all-new \
                                     or all-already-persisted)",
                                    aggregate_type, aggregate_id,
                                ));
                            };
                            let position: i64 = row.try_get("position")?;
                            let revision: Option<i64> = row.try_get("revision")?;
                            return Ok(WriteResult {
                                position: LogCursor::from_raw(position as u64),
                                revision: StreamRevision::from_raw(
                                    revision.unwrap_or(0) as u64,
                                ),
                            });
                        }
                        if constraint == Some("idx_causal_log_stream") {
                            drop(tx);
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
                            // Typed ConflictError so Engine::append can
                            // downcast + retry (not a bare string).
                            return Err(anyhow::Error::new(ConflictError {
                                expected,
                                current: current
                                    .map(|c| StreamRevision::from_raw(c as u64)),
                            }));
                        }
                        return Err(e.into());
                    }
                }
            }

            tx.commit().await?;
            Ok(result)
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
        let created_at: DateTime<Utc> = row.try_get("created_at")?;

        // Aggregate identity is all-present (the current model: every event
        // belongs to a stream) or all-NULL (legacy non-aggregate rows). A
        // half-populated row is corruption — fail loudly rather than silently
        // mis-attribute the event to the nil stream at revision 0.
        let category: Option<String> = row.try_get("aggregate_type")?;
        let stream_id: Option<Uuid> = row.try_get("aggregate_id")?;
        let revision: Option<i64> = row.try_get("revision")?;
        let (category, stream_id, revision) = match (category, stream_id, revision) {
            (Some(c), Some(s), Some(r)) => (c, s, r),
            (None, None, None) => (String::new(), Uuid::nil(), 0),
            _ => {
                return Err(anyhow::anyhow!(
                    "causal_log position {position}: half-populated aggregate \
                     identity — aggregate_type/aggregate_id/revision must be \
                     all-set or all-NULL"
                ))
            }
        };

        Ok(RecordedEvent {
            position: LogCursor::from_raw(position as u64),
            event_id: row.try_get("event_id")?,
            causation_id: row.try_get("causation_id")?,
            correlation_id: row.try_get("correlation_id")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
            category,
            stream_id,
            revision: StreamRevision::from_raw(revision as u64),
            metadata,
            created_at,
            ephemeral: None,
            persistent: row.try_get("persistent")?,
        })
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgEventLogBackend;
