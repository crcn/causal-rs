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

#[cfg(feature = "postgres")]
mod pg {
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

    enum Rec {
        Started {
            event_id: Uuid,
            reactor_id: String,
            correlation_id: Uuid,
            attempt: i32,
            started_at: DateTime<Utc>,
        },
        Finished {
            event_id: Uuid,
            reactor_id: String,
            correlation_id: Uuid,
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
            correlation_id: Uuid,
            attempts: i32,
            error: String,
            at: DateTime<Utc>,
        },
        Aggregate {
            event_id: Uuid,
            aggregate_key: String,
            correlation_id: Uuid,
            state: Value,
        },
        Description {
            event_id: Uuid,
            reactor_id: String,
            correlation_id: Uuid,
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
            correlation_id: Uuid,
            attempt: u32,
            started_at: DateTime<Utc>,
        ) {
            self.send(Rec::Started {
                event_id,
                reactor_id: reactor_id.to_string(),
                correlation_id,
                attempt: attempt as i32,
                started_at,
            });
        }

        fn reactor_completed(
            &self,
            event_id: Uuid,
            reactor_id: &str,
            correlation_id: Uuid,
            attempt: u32,
            started_at: DateTime<Utc>,
            completed_at: DateTime<Utc>,
            logs: &[LogEntry],
        ) {
            self.send(Rec::Finished {
                event_id,
                reactor_id: reactor_id.to_string(),
                correlation_id,
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
            correlation_id: Uuid,
            attempt: u32,
            started_at: DateTime<Utc>,
            completed_at: DateTime<Utc>,
            error: &str,
            logs: &[LogEntry],
        ) {
            self.send(Rec::Finished {
                event_id,
                reactor_id: reactor_id.to_string(),
                correlation_id,
                attempt: attempt as i32,
                status: "failed",
                error: Some(error.to_string()),
                started_at,
                completed_at,
                logs: logs.to_vec(),
            });
        }

        fn reactor_dlq(
            &self,
            event_id: Uuid,
            reactor_id: &str,
            correlation_id: Uuid,
            attempts: u32,
            error: &str,
            at: DateTime<Utc>,
        ) {
            self.send(Rec::Dlq {
                event_id,
                reactor_id: reactor_id.to_string(),
                correlation_id,
                attempts: attempts as i32,
                error: error.to_string(),
                at,
            });
        }

        fn aggregate_folded(
            &self,
            correlation_id: Uuid,
            _position: LogCursor,
            event_id: Uuid,
            aggregate_key: &str,
            state: Value,
        ) {
            self.send(Rec::Aggregate {
                event_id,
                aggregate_key: aggregate_key.to_string(),
                correlation_id,
                state,
            });
        }

        fn reactor_description(
            &self,
            correlation_id: Uuid,
            _position: LogCursor,
            event_id: Uuid,
            reactor_id: &str,
            description: Value,
        ) {
            self.send(Rec::Description {
                event_id,
                reactor_id: reactor_id.to_string(),
                correlation_id,
                description,
            });
        }
    }

    async fn writer_loop(pool: PgPool, mut rx: mpsc::Receiver<Rec>) {
        loop {
            let Some(first) = rx.recv().await else { break };
            let mut batch = vec![first];
            while batch.len() < BATCH_MAX {
                match rx.try_recv() {
                    Ok(rec) => batch.push(rec),
                    Err(_) => break,
                }
            }
            if let Err(e) = flush(&pool, &batch).await {
                tracing::warn!(error = %e, "PgReactorObserver: dropping a batch of {} records", batch.len());
            }
        }
    }

    async fn flush(pool: &PgPool, batch: &[Rec]) -> anyhow::Result<()> {
        let mut tx = pool.begin().await?;
        for rec in batch {
            match rec {
                Rec::Started { event_id, reactor_id, correlation_id, attempt, started_at } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_executions
                            (event_id, reactor_id, attempt, correlation_id, status, started_at)
                         VALUES ($1, $2, $3, $4, 'running', $5)
                         ON CONFLICT (event_id, reactor_id, attempt) DO NOTHING",
                    )
                    .bind(event_id).bind(reactor_id).bind(attempt).bind(correlation_id).bind(started_at)
                    .execute(&mut *tx).await?;
                }
                Rec::Finished {
                    event_id, reactor_id, correlation_id, attempt, status, error, started_at, completed_at, logs,
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
                    .bind(event_id).bind(reactor_id).bind(attempt).bind(correlation_id)
                    .bind(status).bind(error).bind(started_at).bind(completed_at)
                    .execute(&mut *tx).await?;

                    for (ord, log) in logs.iter().enumerate() {
                        sqlx::query(
                            "INSERT INTO causal_reactor_logs
                                (event_id, reactor_id, attempt, ord, correlation_id, level, message, data, logged_at)
                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                             ON CONFLICT (event_id, reactor_id, attempt, ord) DO NOTHING",
                        )
                        .bind(event_id).bind(reactor_id).bind(attempt).bind(ord as i32).bind(correlation_id)
                        .bind(level_str(&log.level)).bind(&log.message).bind(&log.data).bind(log.timestamp)
                        .execute(&mut *tx).await?;
                    }
                }
                Rec::Dlq { event_id, reactor_id, correlation_id, attempts, error, at } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_executions
                            (event_id, reactor_id, attempt, correlation_id, status, error, started_at, completed_at)
                         VALUES ($1, $2, $3, $4, 'dead_letter', $5, $6, $6)
                         ON CONFLICT (event_id, reactor_id, attempt) DO UPDATE
                           SET status = 'dead_letter',
                               error = EXCLUDED.error,
                               completed_at = EXCLUDED.completed_at",
                    )
                    .bind(event_id).bind(reactor_id).bind(attempts).bind(correlation_id).bind(error).bind(at)
                    .execute(&mut *tx).await?;
                }
                Rec::Aggregate { event_id, aggregate_key, correlation_id, state } => {
                    sqlx::query(
                        "INSERT INTO causal_aggregate_snapshots
                            (event_id, aggregate_key, correlation_id, state)
                         VALUES ($1, $2, $3, $4)
                         ON CONFLICT (event_id, aggregate_key) DO NOTHING",
                    )
                    .bind(event_id).bind(aggregate_key).bind(correlation_id).bind(state)
                    .execute(&mut *tx).await?;
                }
                Rec::Description { event_id, reactor_id, correlation_id, description } => {
                    sqlx::query(
                        "INSERT INTO causal_reactor_descriptions
                            (event_id, reactor_id, correlation_id, description)
                         VALUES ($1, $2, $3, $4)
                         ON CONFLICT (event_id, reactor_id) DO NOTHING",
                    )
                    .bind(event_id).bind(reactor_id).bind(correlation_id).bind(description)
                    .execute(&mut *tx).await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgReactorObserver;
