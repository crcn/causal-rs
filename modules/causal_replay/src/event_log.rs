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
//! - **C6 (aggregate OCC):** `StreamRevision(n)` validates the actual
//!   head inside the (advisory-lock-serialized) transaction — an
//!   expectation AHEAD of the head is rejected instead of silently
//!   creating a revision hole, and an expectation BEHIND the head is
//!   reconciled with the shared [`crate::reconcile`] helper first
//!   (crash-redelivered batches return their original `WriteResult`;
//!   partial overlaps fail loudly) before surfacing a typed
//!   `ConflictError`. The partial
//!   `UNIQUE(aggregate_type, aggregate_id, revision)` index remains
//!   the belt-and-braces backstop for the `NoStream` path.
//!
//! ## Position ordering and gap visibility
//!
//! `position` is a BIGSERIAL assigned at INSERT time, but concurrent
//! transactions commit in arbitrary order. Unserialized, a tailer doing
//! `WHERE position > $1 ORDER BY position` can read AND checkpoint
//! position 11 while the transaction holding position 10 is still in
//! flight; when that transaction later commits, position 10 sits
//! permanently behind every cursor — silent, unrecoverable event loss.
//!
//! Fix: every `causal_log` writer (this backend's `append_to_stream`
//! and `PgEventProjector`'s mirror inserts) takes a transaction-scoped
//! global advisory lock (`pg_advisory_xact_lock`, released at
//! COMMIT/ROLLBACK) before assigning positions. Writers are
//! serialized, so commit order == position order: once a tailer has
//! observed position N, every position <= N is either already visible
//! or belongs to a rolled-back transaction. The remaining BIGSERIAL
//! gaps (rollbacks) are harmless — `position > cursor` skips them and
//! nothing can ever materialize inside them. `latest_position()` is a
//! trustworthy high-water mark for the same reason.
//!
//! Rejected alternative: xmin fencing on the read side (cap `read_all`
//! below the oldest in-flight writer's snapshot xmin). More write
//! concurrency, but materially more complexity — per-read snapshot
//! plumbing, and any long-lived transaction stalls every tailer.
//! Revisit only if the advisory lock shows up in profiles; the
//! production append path runs on Kurrent, where this lock costs
//! nothing.

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
    use causal::event_log::{ConflictError, DivergentRedelivery};
    use causal::EventLogBackend;

    use crate::reconcile::{reconcile, Reconciliation};

    /// Advisory-lock key serializing all `causal_log` writers (see the
    /// module doc, "Position ordering and gap visibility").
    ///
    /// Why the two-arg `pg_advisory_xact_lock(classid int4, objid int4)`
    /// form: the database may be shared infrastructure, and other
    /// subsystems take advisory locks of their own. The single-arg form
    /// keys on one bare i64, so two independent users picking the same
    /// small integer collide silently and serialize against each other.
    /// The two-arg form gives us a namespaced (classid, objid) pair:
    /// `0xCA05` ("CAuSal") is the class for every causal-rs lock, and
    /// `0xA1` names this one — the append-serialization lock (audit
    /// item A1).
    ///
    /// Public so operators (and tests) can coordinate with — or
    /// observe — the lock from outside the library, e.g. via
    /// `pg_locks WHERE locktype = 'advisory' AND classid = … AND objid = …`.
    pub const ADVISORY_LOCK_CLASS: i32 = 0xCA05;
    pub const ADVISORY_LOCK_OBJID: i32 = 0xA1;

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

    /// A dedup-hit must be a **byte-identical** redelivery (see the
    /// `EventLogBackend` idempotency contract): a persisted row whose
    /// `payload` / `event_type` / `workflow_id` / `causation_id` — or
    /// placement (`aggregate_type` / `aggregate_id`) — differs from the
    /// redelivered batch means the producer re-decided differently on
    /// redelivery — error loudly instead of silently keeping the old row.
    /// `created_at` and `metadata` are exempt (documented hints that
    /// redeliveries re-stamp).
    ///
    /// One round-trip: UNNEST the batch, join on `event_id`, return the
    /// first divergent id (if any). The global `UNIQUE(event_id)` is what
    /// makes a cross-stream id reuse reachable here (Postgres can enforce
    /// placement identity where the per-stream Kurrent scan cannot).
    async fn ensure_redelivery_identical<'e, E>(
        executor: E,
        aggregate_type: &str,
        aggregate_id: Uuid,
        event_ids: &[Uuid],
        event_types: &[String],
        payloads: &[serde_json::Value],
        workflow_ids: &[Uuid],
        causation_ids: &[Option<Uuid>],
    ) -> Result<()>
    where
        E: sqlx::PgExecutor<'e>,
    {
        // Placement (aggregate_type/aggregate_id) is part of identity: the
        // batch's target stream is scalar, so the same event_id persisted
        // under a DIFFERENT stream means the producer routed one logical
        // event to two subjects — divergence, not a dedup-hit. The global
        // UNIQUE(event_id) makes the cross-stream reuse reachable here.
        let divergent: Option<Uuid> = sqlx::query_scalar(
            "SELECT b.event_id
               FROM UNNEST($1::uuid[], $2::text[], $3::jsonb[],
                           $4::uuid[], $5::uuid[])
                    AS b(event_id, event_type, payload,
                         correlation_id, causation_id)
               JOIN causal_log e ON e.event_id = b.event_id
              WHERE e.event_type     IS DISTINCT FROM b.event_type
                 OR e.payload        IS DISTINCT FROM b.payload
                 OR e.correlation_id IS DISTINCT FROM b.correlation_id
                 OR e.causation_id   IS DISTINCT FROM b.causation_id
                 OR e.aggregate_type IS DISTINCT FROM $6
                 OR e.aggregate_id   IS DISTINCT FROM $7
              LIMIT 1",
        )
        .bind(event_ids)
        .bind(event_types)
        .bind(payloads)
        .bind(workflow_ids)
        .bind(causation_ids)
        .bind(aggregate_type)
        .bind(aggregate_id)
        .fetch_optional(executor)
        .await?;
        if let Some(id) = divergent {
            // Typed so the reactor runner can tell this apart from genuine
            // I/O by downcast (it accepts the persisted row and shouts
            // rather than retrying forever). The SQL locates the divergent
            // id, not the field; name the compared set on the error.
            return Err(anyhow::Error::new(DivergentRedelivery {
                event_id: id,
                diff:     "payload/event_type/workflow/causation/placement".to_string(),
            }));
        }
        Ok(())
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

            // Build all row data BEFORE the transaction: the global
            // advisory lock below serializes every writer, so the time
            // between `begin()` and `commit()` must be O(1) round-trips
            // of pure SQL — no per-event encoding work under the lock.
            let event_ids:       Vec<Uuid>           = events.iter().map(|e| e.event_id).collect();
            let causation_ids:   Vec<Option<Uuid>>   = events.iter().map(|e| e.causation_id).collect();
            let workflow_ids: Vec<Uuid>           = events.iter().map(|e| e.workflow_id).collect();
            let event_types:     Vec<String>         = events.iter().map(|e| e.event_type.clone()).collect();
            let payloads:        Vec<serde_json::Value> = events.iter().map(|e| e.payload.clone()).collect();
            let metadatas:       Vec<serde_json::Value> = events
                .iter()
                .map(|e| serde_json::Value::Object(e.metadata.clone()))
                .collect();
            let created_ats:     Vec<DateTime<Utc>>  = events.iter().map(|e| e.created_at).collect();
            let persistents:     Vec<bool>           = events.iter().map(|e| e.persistent).collect();

            // One transaction so the whole batch lands atomically: a
            // partial multi-fact decision is never observable, and a
            // mid-batch failure rolls back cleanly.
            let mut tx = self.pool.begin().await?;

            // Serialize all causal_log writers (module doc, "Position
            // ordering and gap visibility"): with appends serialized,
            // commit order == position order, so `position > cursor`
            // tailing never checkpoints past an in-flight position and
            // `latest_position()` is a real high-water mark. The lock
            // is transaction-scoped — released at COMMIT/ROLLBACK,
            // including the early-return drops below.
            sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
                .bind(ADVISORY_LOCK_CLASS)
                .bind(ADVISORY_LOCK_OBJID)
                .execute(&mut *tx)
                .await?;

            // Revision of the FIRST event in the batch (0-indexed); the
            // rest follow at consecutive revisions.
            //   NoStream         → 0
            //   StreamRevision(n)→ n + 1, AFTER validating head == n
            //   StreamExists/Any → read the current tail first.
            let base_revision: i64 = match expected {
                StreamState::NoStream => 0,
                StreamState::StreamRevision(n) => {
                    // A3: validate the head INSIDE the lock-serialized
                    // transaction. Without this, an expected revision
                    // AHEAD of the head would silently insert at n+1
                    // and leave a revision hole (the unique stream
                    // index only catches expectations BEHIND the head).
                    let head: Option<i64> = sqlx::query_scalar(
                        "SELECT MAX(revision)
                           FROM causal_log
                          WHERE aggregate_type = $1
                            AND aggregate_id = $2",
                    )
                    .bind(aggregate_type)
                    .bind(aggregate_id)
                    .fetch_one(&mut *tx)
                    .await?;
                    if head == Some(n as i64) {
                        (n + 1) as i64
                    } else {
                        // Head mismatch. Before conflicting, check
                        // whether this batch ALREADY landed on an
                        // earlier attempt (crash-redelivery re-appends
                        // the identical batch at the ORIGINAL expected
                        // revision; the head validation fires before
                        // the event_id unique constraint ever would).
                        // The conflict window is everything after the
                        // expectation; the shared `reconcile` helper
                        // classifies the batch against it.
                        let window_ids: Vec<Uuid> = sqlx::query_scalar(
                            "SELECT event_id
                               FROM causal_log
                              WHERE aggregate_type = $1
                                AND aggregate_id = $2
                                AND revision > $3
                              ORDER BY revision ASC",
                        )
                        .bind(aggregate_type)
                        .bind(aggregate_id)
                        .bind(n as i64)
                        .fetch_all(&mut *tx)
                        .await?;
                        match reconcile(&event_ids, &window_ids) {
                            Reconciliation::Redelivery => {
                                // Whole batch already persisted —
                                // verify it's byte-identical, then
                                // return the ORIGINAL WriteResult (its
                                // last event's coordinates), mirroring
                                // the event_id-dedup lookup below.
                                ensure_redelivery_identical(
                                    &mut *tx,
                                    aggregate_type,
                                    aggregate_id,
                                    &event_ids,
                                    &event_types,
                                    &payloads,
                                    &workflow_ids,
                                    &causation_ids,
                                )
                                .await?;
                                let row = sqlx::query(
                                    "SELECT position, revision FROM causal_log \
                                     WHERE event_id = $1",
                                )
                                .bind(last_event_id)
                                .fetch_one(&mut *tx)
                                .await?;
                                let position: i64 = row.try_get("position")?;
                                let revision: i64 = row.try_get("revision")?;
                                drop(tx); // rollback — releases the advisory lock
                                return Ok(WriteResult {
                                    position: LogCursor::from_raw(position as u64),
                                    revision: StreamRevision::from_raw(revision as u64),
                                });
                            }
                            Reconciliation::PartialOverlap => {
                                drop(tx); // rollback — releases the advisory lock
                                return Err(anyhow::anyhow!(
                                    "append_to_stream on {}:{}: batch partially \
                                     overlaps an earlier append — an event_id \
                                     already exists but the full batch does not \
                                     (event_ids must be all-new or \
                                     all-already-persisted)",
                                    aggregate_type, aggregate_id,
                                ));
                            }
                            Reconciliation::Conflict => {
                                drop(tx); // rollback — releases the advisory lock
                                // Typed ConflictError so Engine::append
                                // can downcast + retry; current is None
                                // for an empty stream (matching
                                // MemoryStore).
                                return Err(anyhow::Error::new(ConflictError {
                                    expected,
                                    current: head
                                        .map(|c| StreamRevision::from_raw(c as u64)),
                                }));
                            }
                        }
                    }
                }
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

            let revisions: Vec<i64> =
                (0..events.len() as i64).map(|i| base_revision + i).collect();

            // Single multi-row INSERT: one round-trip while holding the
            // global advisory lock (lock-span discipline). WITH
            // ORDINALITY + ORDER BY pins insert order to batch order,
            // so positions ascend with revisions within the batch.
            let rows = sqlx::query(
                // Storage keeps its native column name: the domain's
                // `workflow_id` lands in `correlation_id` (0.10 step 1's
                // boundary rule — vocabulary stops at EventLogBackend).
                "INSERT INTO causal_log
                    (event_id, causation_id, correlation_id, event_type,
                     payload, aggregate_type, aggregate_id, revision,
                     metadata, created_at, persistent)
                 SELECT u.event_id, u.causation_id, u.correlation_id,
                        u.event_type, u.payload, $1, $2, u.revision,
                        u.metadata, u.created_at, u.persistent
                   FROM UNNEST($3::uuid[], $4::uuid[], $5::uuid[],
                               $6::text[], $7::jsonb[], $8::bigint[],
                               $9::jsonb[], $10::timestamptz[],
                               $11::boolean[])
                        WITH ORDINALITY
                        AS u(event_id, causation_id, correlation_id,
                             event_type, payload, revision, metadata,
                             created_at, persistent, ord)
                  ORDER BY u.ord
                 RETURNING position, revision",
            )
            .bind(aggregate_type)
            .bind(aggregate_id)
            .bind(&event_ids)
            .bind(&causation_ids)
            .bind(&workflow_ids)
            .bind(&event_types)
            .bind(&payloads)
            .bind(&revisions)
            .bind(&metadatas)
            .bind(&created_ats)
            .bind(&persistents)
            .fetch_all(&mut *tx)
            .await;

            match rows {
                Ok(rows) => {
                    // WriteResult carries the LAST event's position +
                    // revision. Pick it by max revision rather than
                    // trusting RETURNING row order.
                    let mut result = WriteResult {
                        position: LogCursor::ZERO,
                        revision: StreamRevision::from_raw(0),
                    };
                    let mut best: i64 = -1;
                    for row in &rows {
                        let position: i64 = row.try_get("position")?;
                        let revision: i64 = row.try_get("revision")?;
                        if revision > best {
                            best = revision;
                            result = WriteResult {
                                position: LogCursor::from_raw(position as u64),
                                revision: StreamRevision::from_raw(revision as u64),
                            };
                        }
                    }
                    tx.commit().await?;
                    Ok(result)
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
                        drop(tx); // rollback — releases the advisory lock
                        // Idempotent retry: the batch was already written, so
                        // its last event is present — return its result. If
                        // the last event is ABSENT, only part of the batch
                        // previously landed: a partial-overlap batch that
                        // violates the all-new-or-all-present precondition.
                        // Fail with a clear error rather than the opaque
                        // RowNotFound a `fetch_one` would raise.
                        ensure_redelivery_identical(
                            &self.pool,
                            aggregate_type,
                            aggregate_id,
                            &event_ids,
                            &event_types,
                            &payloads,
                            &workflow_ids,
                            &causation_ids,
                        )
                        .await?;
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
                        drop(tx); // rollback — releases the advisory lock
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
        let created_at: DateTime<Utc> = row.try_get("created_at")?;

        // Aggregate identity is all-present (the current model: every event
        // belongs to a stream) or all-NULL (legacy non-aggregate rows). A
        // half-populated row is corruption — fail loudly rather than silently
        // mis-attribute the event to the nil stream at revision 0.
        let category: Option<String> = row.try_get("aggregate_type")?;
        let subject_id: Option<Uuid> = row.try_get("aggregate_id")?;
        let revision: Option<i64> = row.try_get("revision")?;
        let (category, subject_id, revision) = match (category, subject_id, revision) {
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
            workflow_id: row.try_get("correlation_id")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
            category,
            subject_id,
            revision: StreamRevision::from_raw(revision as u64),
            metadata,
            created_at,
            ephemeral: None,
            persistent: row.try_get("persistent")?,
        })
    }
}

#[cfg(feature = "postgres")]
pub use pg::{PgEventLogBackend, ADVISORY_LOCK_CLASS, ADVISORY_LOCK_OBJID};
