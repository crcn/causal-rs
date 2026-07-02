//! Best-effort Postgres sink for reactor observability (`PgReactorObserver`).
//!
//! Implements `causal::ReactorObserver`. The hooks run on the engine hot path,
//! so they only `try_send` onto a bounded channel — **lossy by design** (drop on
//! overflow). A spawned background task drains the channel in batches and writes
//! them in one transaction with `ON CONFLICT` upserts. KurrentDB stays the
//! durable source of truth; these tables are a fleet-shared read model for the
//! inspector, so dropping a record just means the inspector is briefly behind.
//!
//! Pairs with `PgInspectorReadModel` (read side) over the same tables.
//!
//! **Fail-soft writes.** The whole batch is written in one transaction on the
//! fast path; if that fails, the writer falls back to writing each record in
//! its own transaction, so one failing record class (e.g. a table missing
//! because a migration shipped after the consumer's last schema apply) drops
//! only that record instead of poisoning the co-batched execution/log rows. A
//! missing table (SQLSTATE 42P01) is warned once per table.
//!
//! **Schema ownership.** The crate owns its DDL: apply
//! [`INSPECTOR_SCHEMA_SQL`] through a migration pipeline, or call
//! [`PgReactorObserver::ensure_schema`] / construct with
//! [`PgReactorObserver::new_with_ensure_schema`]. `new` never runs DDL.

#[cfg(feature = "postgres")]
mod pg {
    use std::collections::HashSet;

    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use sqlx::PgPool;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use causal::reactor_observer::ReactorObserver;
    use causal::types::{LogCursor, LogEntry, LogLevel};

    /// Bound on the in-flight buffer; full channel → drop (best-effort).
    const CHANNEL_CAP: usize = 8192;
    /// Max records coalesced into one write transaction.
    const BATCH_MAX: usize = 512;

    /// Postgres SQLSTATE for `undefined_table` — the code raised when the
    /// consumer's database is missing an inspector table (typically because a
    /// migration shipped after their last schema apply). Handled fail-soft:
    /// warned once per table, never allowed to poison co-batched records.
    const UNDEFINED_TABLE: &str = "42P01";

    /// The five inspector-read-model tables this observer writes, as an
    /// idempotent (`IF NOT EXISTS`) schema. Exported so consumers with a
    /// migration pipeline can apply it through their own tooling; also run by
    /// [`PgReactorObserver::ensure_schema`]. Kept in sync with
    /// `migrations/20260608_reactor_observability.sql`,
    /// `migrations/20260628_reactor_divergences.sql`, and `docs/schema.sql`.
    pub const INSPECTOR_SCHEMA_SQL: &str = include_str!("sql/inspector_schema.sql");

    enum Rec {
        Started {
            event_id: Uuid,
            reactor_id: String,
            workflow_id: Uuid,
            attempt: i32,
            started_at: DateTime<Utc>,
        },
        Finished {
            event_id: Uuid,
            reactor_id: String,
            workflow_id: Uuid,
            attempt: i32,
            status: &'static str,
            error: Option<String>,
            started_at: DateTime<Utc>,
            completed_at: DateTime<Utc>,
            logs: Vec<LogEntry>,
        },
        Dlq {
            event_id: Uuid,
            reactor_id: String,
            workflow_id: Uuid,
            attempts: i32,
            error: String,
            at: DateTime<Utc>,
        },
        Divergence {
            event_id: Uuid,
            reactor_id: String,
            workflow_id: Uuid,
            diff: String,
            at: DateTime<Utc>,
        },
        Aggregate {
            event_id: Uuid,
            aggregate_key: String,
            workflow_id: Uuid,
            state: Value,
        },
        Description {
            event_id: Uuid,
            reactor_id: String,
            workflow_id: Uuid,
            description: Value,
        },
    }

    /// Best-effort Postgres reactor-observability sink. Construct inside a Tokio
    /// runtime (it spawns the background writer) and wire it with
    /// `EngineBuilder::with_observer(Arc::new(PgReactorObserver::new(pool)))`.
    pub struct PgReactorObserver {
        tx: mpsc::Sender<Rec>,
    }

    impl PgReactorObserver {
        pub fn new(pool: PgPool) -> Self {
            let (tx, rx) = mpsc::channel(CHANNEL_CAP);
            tokio::spawn(writer_loop(pool, rx));
            Self { tx }
        }

        /// Idempotently create every inspector table this observer writes
        /// (`CREATE TABLE / INDEX IF NOT EXISTS`; see [`INSPECTOR_SCHEMA_SQL`]).
        /// Safe to call on every boot. Never run implicitly by [`new`] — apps
        /// with a migration pipeline should apply [`INSPECTOR_SCHEMA_SQL`]
        /// there instead of calling this at runtime.
        ///
        /// [`new`]: Self::new
        pub async fn ensure_schema(pool: &PgPool) -> anyhow::Result<()> {
            sqlx::raw_sql(INSPECTOR_SCHEMA_SQL).execute(pool).await?;
            Ok(())
        }

        /// [`ensure_schema`] then [`new`] — convenience for apps without a
        /// migration pipeline. Fails if the DDL cannot be applied.
        ///
        /// [`ensure_schema`]: Self::ensure_schema
        /// [`new`]: Self::new
        pub async fn new_with_ensure_schema(pool: PgPool) -> anyhow::Result<Self> {
            Self::ensure_schema(&pool).await?;
            Ok(Self::new(pool))
        }

        fn send(&self, rec: Rec) {
            // Hot path: never block, never error — drop if the buffer is full.
            let _ = self.tx.try_send(rec);
        }
    }

    fn level_str(level: &LogLevel) -> &'static str {
        match level {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
        }
    }

    impl ReactorObserver for PgReactorObserver {
        fn reactor_started(
            &self,
            event_id: Uuid,
            reactor_id: &str,
            workflow_id: Uuid,
            attempt: u32,
            started_at: DateTime<Utc>,
        ) {
            self.send(Rec::Started {
                event_id,
                reactor_id: reactor_id.to_string(),
                workflow_id,
                attempt: attempt as i32,
                started_at,
            });
        }

        fn reactor_completed(
            &self,
            event_id: Uuid,
            reactor_id: &str,
            workflow_id: Uuid,
            attempt: u32,
            started_at: DateTime<Utc>,
            completed_at: DateTime<Utc>,
            logs: &[LogEntry],
        ) {
            self.send(Rec::Finished {
                event_id,
                reactor_id: reactor_id.to_string(),
                workflow_id,
                attempt: attempt as i32,
                status: "completed",
                error: None,
                started_at,
                completed_at,
                logs: logs.to_vec(),
            });
        }

        fn reactor_failed(
            &self,
            event_id: Uuid,
            reactor_id: &str,
            workflow_id: Uuid,
            attempt: u32,
            started_at: DateTime<Utc>,
            completed_at: DateTime<Utc>,
            error: &str,
            logs: &[LogEntry],
        ) {
            self.send(Rec::Finished {
                event_id,
                reactor_id: reactor_id.to_string(),
                workflow_id,
                attempt: attempt as i32,
                status: "failed",
                error: Some(error.to_string()),
                started_at,
                completed_at,
                logs: logs.to_vec(),
            });
        }

        fn reactor_terminal_failure(
            &self,
            event_id: Uuid,
            reactor_id: &str,
            workflow_id: Uuid,
            attempts: u32,
            error: &str,
            at: DateTime<Utc>,
        ) {
            self.send(Rec::Dlq {
                event_id,
                reactor_id: reactor_id.to_string(),
                workflow_id,
                attempts: attempts as i32,
                error: error.to_string(),
                at,
            });
        }

        fn reactor_divergence(
            &self,
            event_id: Uuid,
            reactor_id: &str,
            workflow_id: Uuid,
            diff: &str,
        ) {
            // Recorded in a table apart from causal_reactor_executions: a
            // divergent redelivery is NOT a failure status (the canonical
            // output stands, the runner advanced), it's a determinism
            // warning surfaced as a `diverged` flag. The hook carries no
            // timestamp, so stamp at send.
            self.send(Rec::Divergence {
                event_id,
                reactor_id: reactor_id.to_string(),
                workflow_id,
                diff: diff.to_string(),
                at: Utc::now(),
            });
        }

        fn aggregate_folded(
            &self,
            workflow_id: Uuid,
            _position: LogCursor,
            event_id: Uuid,
            aggregate_key: &str,
            state: Value,
        ) {
            self.send(Rec::Aggregate {
                event_id,
                aggregate_key: aggregate_key.to_string(),
                workflow_id,
                state,
            });
        }

        fn reactor_description(
            &self,
            workflow_id: Uuid,
            _position: LogCursor,
            event_id: Uuid,
            reactor_id: &str,
            description: Value,
        ) {
            self.send(Rec::Description {
                event_id,
                reactor_id: reactor_id.to_string(),
                workflow_id,
                description,
            });
        }
    }

    async fn writer_loop(pool: PgPool, mut rx: mpsc::Receiver<Rec>) {
        // Tables we've already warned about being missing — rate-limits the
        // 42P01 warning to once per table for the life of the writer task.
        let mut warned_tables: HashSet<String> = HashSet::new();
        loop {
            let Some(first) = rx.recv().await else { break };
            let mut batch = vec![first];
            while batch.len() < BATCH_MAX {
                match rx.try_recv() {
                    Ok(rec) => batch.push(rec),
                    Err(_) => break,
                }
            }
            // Fast path: the whole batch in one transaction. Only on failure
            // do we fall back to per-record writes, so one failing record
            // class (e.g. a missing table) cannot poison the co-batched
            // execution/log rows.
            if let Err(e) = flush(&pool, &batch).await {
                tracing::debug!(
                    error = %e,
                    "PgReactorObserver: batch write failed; retrying per record",
                );
                flush_per_record(&pool, &batch, &mut warned_tables).await;
            }
        }
    }

    /// Fast path: write the whole batch atomically. Any error aborts the
    /// transaction and bubbles up so [`writer_loop`] can retry per record.
    async fn flush(pool: &PgPool, batch: &[Rec]) -> anyhow::Result<()> {
        let mut tx = pool.begin().await?;
        for rec in batch {
            write_rec(&mut tx, rec).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Degraded path: write each record in its own small transaction so a
    /// record class that always fails (typically a missing table) drops only
    /// that record, not the rest of the batch. Every insert is an `ON
    /// CONFLICT` upsert, so re-writing records that already committed on the
    /// aborted fast-path transaction is idempotent.
    async fn flush_per_record(pool: &PgPool, batch: &[Rec], warned_tables: &mut HashSet<String>) {
        let mut dropped = 0usize;
        for rec in batch {
            let attempt: anyhow::Result<()> = async {
                let mut tx = pool.begin().await?;
                write_rec(&mut tx, rec).await?;
                tx.commit().await?;
                Ok(())
            }
            .await;
            if let Err(e) = attempt {
                dropped += 1;
                let table = table_for(rec);
                // write_rec wraps the sqlx error in anyhow — downcast back to
                // read the SQLSTATE (`as_database_error` is a sqlx method).
                let missing_table = e
                    .downcast_ref::<sqlx::Error>()
                    .and_then(|se| se.as_database_error())
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == UNDEFINED_TABLE);
                if missing_table && warned_tables.insert(table.to_string()) {
                    tracing::warn!(
                        table,
                        "PgReactorObserver: table `{table}` is missing — inspector \
                         records for it are being dropped. Apply \
                         `causal_replay::INSPECTOR_SCHEMA_SQL` (or call \
                         `PgReactorObserver::ensure_schema`) to provision it. This \
                         warning is logged once per table.",
                    );
                } else if !missing_table {
                    tracing::debug!(error = %e, table, "PgReactorObserver: dropping a record");
                }
            }
        }
        if dropped > 0 {
            tracing::warn!(
                "PgReactorObserver: dropped {dropped}/{} records after a batch write failure",
                batch.len(),
            );
        }
    }

    /// The inspector table a record variant writes to first — used to label
    /// and rate-limit missing-table warnings in the degraded path.
    fn table_for(rec: &Rec) -> &'static str {
        match rec {
            Rec::Started { .. } | Rec::Finished { .. } | Rec::Dlq { .. } => {
                "causal_reactor_executions"
            }
            Rec::Divergence { .. } => "causal_reactor_divergences",
            Rec::Aggregate { .. } => "causal_aggregate_snapshots",
            Rec::Description { .. } => "causal_reactor_descriptions",
        }
    }

    /// Write one record's statements on any executor — shared by the batch
    /// fast path and the per-record fallback. SQL is identical to the prior
    /// inline batch (pure extraction).
    async fn write_rec(conn: &mut sqlx::PgConnection, rec: &Rec) -> anyhow::Result<()> {
        match rec {
                Rec::Started { event_id, reactor_id, workflow_id, attempt, started_at } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_executions
                            (event_id, reactor_id, attempt, correlation_id, status, started_at)
                         VALUES ($1, $2, $3, $4, 'running', $5)
                         ON CONFLICT (event_id, reactor_id, attempt) DO NOTHING",
                    )
                    .bind(event_id).bind(reactor_id).bind(attempt).bind(workflow_id).bind(started_at)
                    .execute(&mut *conn).await?;
                }
                Rec::Finished {
                    event_id, reactor_id, workflow_id, attempt, status, error, started_at, completed_at, logs,
                } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_executions
                            (event_id, reactor_id, attempt, correlation_id, status, error, started_at, completed_at)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                         ON CONFLICT (event_id, reactor_id, attempt) DO UPDATE
                           SET status = EXCLUDED.status,
                               error = EXCLUDED.error,
                               completed_at = EXCLUDED.completed_at",
                    )
                    .bind(event_id).bind(reactor_id).bind(attempt).bind(workflow_id)
                    .bind(status).bind(error).bind(started_at).bind(completed_at)
                    .execute(&mut *conn).await?;

                    for (ord, log) in logs.iter().enumerate() {
                        sqlx::query(
                            "INSERT INTO causal_reactor_logs
                                (event_id, reactor_id, attempt, ord, correlation_id, level, message, data, logged_at)
                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                             ON CONFLICT (event_id, reactor_id, attempt, ord) DO NOTHING",
                        )
                        .bind(event_id).bind(reactor_id).bind(attempt).bind(ord as i32).bind(workflow_id)
                        .bind(level_str(&log.level)).bind(&log.message).bind(&log.data).bind(log.timestamp)
                        .execute(&mut *conn).await?;
                    }
                }
                Rec::Dlq { event_id, reactor_id, workflow_id, attempts, error, at } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_executions
                            (event_id, reactor_id, attempt, correlation_id, status, error, started_at, completed_at)
                         VALUES ($1, $2, $3, $4, 'dead_letter', $5, $6, $6)
                         ON CONFLICT (event_id, reactor_id, attempt) DO UPDATE
                           SET status = 'dead_letter',
                               error = EXCLUDED.error,
                               completed_at = EXCLUDED.completed_at",
                    )
                    .bind(event_id).bind(reactor_id).bind(attempts).bind(workflow_id).bind(error).bind(at)
                    .execute(&mut *conn).await?;
                }
                Rec::Divergence { event_id, reactor_id, workflow_id, diff, at } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_divergences
                            (event_id, reactor_id, correlation_id, diff, at)
                         VALUES ($1, $2, $3, $4, $5)
                         ON CONFLICT (event_id, reactor_id) DO UPDATE
                           SET diff = EXCLUDED.diff,
                               at = EXCLUDED.at",
                    )
                    .bind(event_id).bind(reactor_id).bind(workflow_id).bind(diff).bind(at)
                    .execute(&mut *conn).await?;
                }
                Rec::Aggregate { event_id, aggregate_key, workflow_id, state } => {
                    sqlx::query(
                        "INSERT INTO causal_aggregate_snapshots
                            (event_id, aggregate_key, correlation_id, state)
                         VALUES ($1, $2, $3, $4)
                         ON CONFLICT (event_id, aggregate_key) DO NOTHING",
                    )
                    .bind(event_id).bind(aggregate_key).bind(workflow_id).bind(state)
                    .execute(&mut *conn).await?;
                }
                Rec::Description { event_id, reactor_id, workflow_id, description } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_descriptions
                            (event_id, reactor_id, correlation_id, description)
                         VALUES ($1, $2, $3, $4)
                         ON CONFLICT (event_id, reactor_id) DO NOTHING",
                    )
                    .bind(event_id).bind(reactor_id).bind(workflow_id).bind(description)
                    .execute(&mut *conn).await?;
                }
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "postgres"))]
mod schema_drift_tests {
    use super::pg::INSPECTOR_SCHEMA_SQL;

    /// The embedded DDL must cover every table the observer writes and stay
    /// idempotent — a cheap guard against the SQL drifting out of sync with
    /// the `flush`/`write_rec` inserts (no database required).
    #[test]
    fn embedded_schema_covers_all_tables_idempotently() {
        for table in [
            "causal_reactor_executions",
            "causal_reactor_logs",
            "causal_reactor_descriptions",
            "causal_aggregate_snapshots",
            "causal_reactor_divergences",
        ] {
            assert!(
                INSPECTOR_SCHEMA_SQL.contains(table),
                "INSPECTOR_SCHEMA_SQL is missing table `{table}`",
            );
        }
        // Idempotency is load-bearing (ensure_schema runs on every boot): no
        // bare CREATE — every one must be guarded with IF NOT EXISTS.
        assert!(
            INSPECTOR_SCHEMA_SQL.contains("CREATE TABLE IF NOT EXISTS"),
            "expected IF NOT EXISTS tables",
        );
        assert!(
            !INSPECTOR_SCHEMA_SQL.contains("CREATE TABLE causal"),
            "found a bare CREATE TABLE — must be CREATE TABLE IF NOT EXISTS",
        );
        assert!(
            !INSPECTOR_SCHEMA_SQL.contains("CREATE INDEX idx"),
            "found a bare CREATE INDEX — must be CREATE INDEX IF NOT EXISTS",
        );
    }
}

#[cfg(feature = "postgres")]
pub use pg::{PgReactorObserver, INSPECTOR_SCHEMA_SQL};
