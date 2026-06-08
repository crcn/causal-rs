//! `PgEventProjector` — best-effort async projection of a source event log
//! (KurrentDB) into PG `causal_log`, so `PgInspectorReadModel`'s event methods
//! have data when Kurrent is the source of truth.
//!
//! Off the hot path: a background catch-up consumer reads `$all` from a cursor
//! stored in `causal_checkpoints` and raw-inserts each event into `causal_log`
//! with `ON CONFLICT (event_id) DO NOTHING` — idempotent, so any/all boxes can
//! run it and a restart simply resumes. PG assigns its own `position` (the read
//! model's single seq authority); the original `event_id` is preserved for
//! dedup + observability joins.

#[cfg(feature = "postgres")]
mod pg {
    use std::sync::Arc;
    use std::time::Duration;

    use sqlx::PgPool;
    use uuid::Uuid;

    use causal::checkpoint_store::CheckpointStore;
    use causal::event_log::EventLogBackend;
    use causal::types::{LogCursor, RecordedEvent};

    const CONSUMER_ID: &str = "__pg_event_projector";
    const BATCH: usize = 256;
    const IDLE_POLL: Duration = Duration::from_millis(200);

    /// Spawns a background task projecting `source`'s `$all` into PG
    /// `causal_log`. `checkpoint` stores the source cursor (use the same PG
    /// pool's `PgReactorCheckpoint` so progress survives restarts).
    pub struct PgEventProjector;

    impl PgEventProjector {
        pub fn spawn(
            source: Arc<dyn EventLogBackend>,
            checkpoint: Arc<dyn CheckpointStore>,
            pool: PgPool,
        ) {
            tokio::spawn(run(source, checkpoint, pool));
        }
    }

    async fn run(
        source: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
        pool: PgPool,
    ) {
        loop {
            let cursor = checkpoint
                .get(CONSUMER_ID)
                .await
                .ok()
                .flatten()
                .unwrap_or(LogCursor::ZERO);

            match source.read_all(cursor, BATCH).await {
                Ok(events) if !events.is_empty() => {
                    let mut last = cursor;
                    for e in &events {
                        if let Err(err) = insert(&pool, e).await {
                            // Best-effort: log and stop advancing past the
                            // failure so the next poll retries it.
                            tracing::warn!(error = %err, "PgEventProjector: insert failed");
                            break;
                        }
                        last = e.position;
                    }
                    if last > cursor {
                        let _ = checkpoint.set(CONSUMER_ID, last).await;
                    }
                }
                Ok(_) => tokio::time::sleep(IDLE_POLL).await,
                Err(err) => {
                    tracing::warn!(error = %err, "PgEventProjector: read_all failed");
                    tokio::time::sleep(IDLE_POLL).await;
                }
            }
        }
    }

    async fn insert(pool: &PgPool, e: &RecordedEvent) -> anyhow::Result<()> {
        let metadata = serde_json::Value::Object(e.metadata.clone());
        // Aggregate identity is all-set or all-NULL (the causal_log invariant a
        // half-populated row would violate). A non-aggregate event deserializes
        // to ("", nil, _); write it back as NULL so it stays out of the partial
        // UNIQUE(aggregate_type, aggregate_id, revision) index — otherwise two
        // such events collide on that index and `ON CONFLICT (event_id)` cannot
        // catch it, erroring the insert and stalling the projector forever.
        let (aggregate_type, aggregate_id, revision): (Option<&str>, Option<Uuid>, Option<i64>) =
            if e.category.is_empty() && e.stream_id.is_nil() {
                (None, None, None)
            } else {
                (Some(e.category.as_str()), Some(e.stream_id), Some(e.revision.raw() as i64))
            };
        sqlx::query(
            "INSERT INTO causal_log
                (event_id, causation_id, correlation_id, event_type, payload,
                 aggregate_type, aggregate_id, revision, metadata, created_at, persistent)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(e.event_id)
        .bind(e.causation_id)
        .bind(e.correlation_id)
        .bind(&e.event_type)
        .bind(&e.payload)
        .bind(aggregate_type)
        .bind(aggregate_id)
        .bind(revision)
        .bind(&metadata)
        .bind(e.created_at)
        .bind(e.persistent)
        .execute(pool)
        .await?;
        Ok(())
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgEventProjector;
