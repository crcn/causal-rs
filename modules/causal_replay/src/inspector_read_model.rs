//! Postgres-backed [`InspectorReadModel`] (`PgInspectorReadModel`).
//!
//! The read side paired with `PgReactorObserver`. Event methods read
//! `causal_log` (populated by the PG event backend / projector); reactor
//! observability methods read the `causal_reactor_*` / `causal_aggregate_*`
//! tables and **join `causal_log` on `event_id`** to derive display `seq`
//! (`causal_log.position` is the single seq authority — there is one seq space).
//!
//! Best-effort: if an observability row's event hasn't been projected to
//! `causal_log` yet, the join drops it until it lands. Semantics mirror the
//! reference `impl InspectorReadModel for MemoryStore`.

#[cfg(feature = "postgres")]
mod pg {
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use causal_inspector::read_model::{
        AggregateLifecycleEntry, AggregateStateSnapshotEntry, CorrelationSummaryEntry, EventQuery,
        InspectorReadModel, ReactorAttemptEntry, ReactorDependencyEntry, ReactorDescriptionEntry,
        ReactorDescriptionSnapshotEntry, ReactorLogEntry, ReactorOutcomeEntry, StoredEvent,
    };

    pub struct PgInspectorReadModel {
        pool: PgPool,
    }

    impl PgInspectorReadModel {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    const EVENT_COLS: &str = "position, event_id, causation_id, correlation_id, event_type, \
        payload, aggregate_type, aggregate_id, revision, metadata, created_at";

    /// Safety bound on correlation-scoped reads. A correlation is normally a
    /// small story, but a pathological/long-running one must not load an
    /// unbounded result set into memory and over the wire from a single
    /// inspector request. Generous enough to never truncate a real chain.
    const MAX_CORRELATION_ROWS: i64 = 10_000;

    fn row_to_stored(row: &sqlx::postgres::PgRow) -> Result<StoredEvent> {
        let position: i64 = row.try_get("position")?;
        let revision: Option<i64> = row.try_get("revision")?;
        let metadata: serde_json::Value = row.try_get("metadata")?;
        let reactor_id = metadata
            .get("reactor_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        Ok(StoredEvent {
            seq: position,
            ts: row.try_get("created_at")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
            id: Some(row.try_get("event_id")?),
            causation_id: row.try_get("causation_id")?,
            correlation_id: Some(row.try_get("correlation_id")?),
            reactor_id,
            aggregate_type: row.try_get("aggregate_type")?,
            aggregate_id: row.try_get("aggregate_id")?,
            stream_revision: revision.map(|r| r as u64),
        })
    }

    /// Parse a correlation filter: `None` → no filter; `Some(valid)` → that id;
    /// `Some(invalid)` → a sentinel nil so nothing matches (mirrors MemoryStore).
    fn parse_corr(s: &str) -> Option<Uuid> {
        Uuid::parse_str(s).ok()
    }

    #[async_trait]
    impl InspectorReadModel for PgInspectorReadModel {
        async fn list_events(&self, query: &EventQuery) -> Result<Vec<StoredEvent>> {
            let limit = query.limit.min(200) as i64;
            // Invalid correlation filter → match nothing (mirror MemoryStore).
            let corr: Option<Uuid> = match &query.correlation_id {
                Some(s) => match parse_corr(s) {
                    Some(id) => Some(id),
                    None => return Ok(vec![]),
                },
                None => None,
            };
            let sql = format!(
                "SELECT {EVENT_COLS} FROM causal_log
                  WHERE ($1::bigint IS NULL OR position < $1)
                    AND ($2::timestamptz IS NULL OR created_at >= $2)
                    AND ($3::timestamptz IS NULL OR created_at <= $3)
                    AND ($4::uuid IS NULL OR correlation_id = $4)
                    AND ($5::text IS NULL OR (aggregate_type || ':' || aggregate_id::text) = $5)
                    AND ($6::text IS NULL OR event_type ILIKE '%'||$6||'%'
                         OR payload::text ILIKE '%'||$6||'%'
                         OR correlation_id::text ILIKE '%'||$6||'%')
                  ORDER BY position DESC
                  LIMIT $7"
            );
            let rows = sqlx::query(&sql)
                .bind(query.cursor)
                .bind(query.from)
                .bind(query.to)
                .bind(corr)
                .bind(&query.aggregate_key)
                .bind(&query.search)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
            rows.iter().map(row_to_stored).collect()
        }

        async fn get_event(&self, seq: i64) -> Result<Option<StoredEvent>> {
            let sql = format!("SELECT {EVENT_COLS} FROM causal_log WHERE position = $1");
            let row = sqlx::query(&sql).bind(seq).fetch_optional(&self.pool).await?;
            row.as_ref().map(row_to_stored).transpose()
        }

        async fn causal_tree(&self, seq: i64) -> Result<(Vec<StoredEvent>, i64)> {
            let sql = format!(
                "SELECT {EVENT_COLS} FROM causal_log
                  WHERE correlation_id = (SELECT correlation_id FROM causal_log WHERE position = $1)
                  ORDER BY position ASC
                  LIMIT {MAX_CORRELATION_ROWS}"
            );
            let rows = sqlx::query(&sql).bind(seq).fetch_all(&self.pool).await?;
            let events: Vec<StoredEvent> = rows.iter().map(row_to_stored).collect::<Result<_>>()?;
            let root_seq = events
                .iter()
                .find(|e| e.causation_id.is_none())
                .map(|e| e.seq)
                .unwrap_or(seq);
            Ok((events, root_seq))
        }

        async fn causal_flow(&self, correlation_id: &str) -> Result<Vec<StoredEvent>> {
            let Some(cid) = parse_corr(correlation_id) else { return Ok(vec![]) };
            let sql = format!(
                "SELECT {EVENT_COLS} FROM causal_log WHERE correlation_id = $1
                  ORDER BY position ASC LIMIT {MAX_CORRELATION_ROWS}"
            );
            let rows = sqlx::query(&sql).bind(cid).fetch_all(&self.pool).await?;
            rows.iter().map(row_to_stored).collect()
        }

        async fn events_from_seq(&self, start_seq: i64, limit: usize) -> Result<Vec<StoredEvent>> {
            let sql = format!(
                "SELECT {EVENT_COLS} FROM causal_log WHERE position >= $1 ORDER BY position ASC LIMIT $2"
            );
            let rows = sqlx::query(&sql)
                .bind(start_seq)
                .bind(limit.min(500) as i64)
                .fetch_all(&self.pool)
                .await?;
            rows.iter().map(row_to_stored).collect()
        }

        async fn reactor_logs(
            &self,
            event_id: Uuid,
            reactor_id: &str,
        ) -> Result<Vec<ReactorLogEntry>> {
            let rows = sqlx::query(
                "SELECT event_id, reactor_id, level, message, data, logged_at
                   FROM causal_reactor_logs
                  WHERE event_id = $1 AND reactor_id = $2
                  ORDER BY attempt ASC, ord ASC",
            )
            .bind(event_id)
            .bind(reactor_id)
            .fetch_all(&self.pool)
            .await?;
            rows.iter()
                .map(|r| {
                    Ok(ReactorLogEntry {
                        event_id: r.try_get("event_id")?,
                        reactor_id: r.try_get("reactor_id")?,
                        level: r.try_get("level")?,
                        message: r.try_get("message")?,
                        data: r.try_get("data")?,
                        logged_at: r.try_get("logged_at")?,
                    })
                })
                .collect()
        }

        async fn reactor_logs_by_correlation(
            &self,
            correlation_id: &str,
        ) -> Result<Vec<ReactorLogEntry>> {
            let Some(cid) = parse_corr(correlation_id) else { return Ok(vec![]) };
            let sql = format!(
                "SELECT event_id, reactor_id, level, message, data, logged_at
                   FROM causal_reactor_logs
                  WHERE correlation_id = $1
                  ORDER BY logged_at ASC
                  LIMIT {MAX_CORRELATION_ROWS}"
            );
            let rows = sqlx::query(&sql).bind(cid).fetch_all(&self.pool).await?;
            rows.iter()
                .map(|r| {
                    Ok(ReactorLogEntry {
                        event_id: r.try_get("event_id")?,
                        reactor_id: r.try_get("reactor_id")?,
                        level: r.try_get("level")?,
                        message: r.try_get("message")?,
                        data: r.try_get("data")?,
                        logged_at: r.try_get("logged_at")?,
                    })
                })
                .collect()
        }

        async fn reactor_outcomes(&self, correlation_id: &str) -> Result<Vec<ReactorOutcomeEntry>> {
            let Some(cid) = parse_corr(correlation_id) else { return Ok(vec![]) };
            // One outcome per reactor: the TERMINAL status (a reactor that failed
            // then recovered is `completed`, not `failed`), its paired error,
            // attempt count, time span, triggering events. The DLQ upsert
            // overwrites the highest-attempt row to `dead_letter`, so the latest
            // attempt's status/error is the true terminal state. `bool_or` is a
            // belt-and-suspenders guard so a dead-letter can never be masked.
            // Mirrors MemoryStore (terminal status; error only when it ended bad).
            let rows = sqlx::query(
                "SELECT reactor_id,
                        bool_or(status = 'dead_letter') AS dead_lettered,
                        (array_agg(status ORDER BY attempt DESC, started_at DESC))[1] AS latest_status,
                        (array_agg(error  ORDER BY attempt DESC, started_at DESC))[1] AS latest_error,
                        COUNT(*) AS attempts,
                        MIN(started_at) AS started_at,
                        MAX(completed_at) AS completed_at,
                        array_agg(DISTINCT event_id::text) AS event_ids
                   FROM causal_reactor_executions
                  WHERE correlation_id = $1
                  GROUP BY reactor_id",
            )
            .bind(cid)
            .fetch_all(&self.pool)
            .await?;
            rows.iter()
                .map(|r| {
                    let dead_lettered: bool = r.try_get("dead_lettered")?;
                    let latest_status: String = r.try_get("latest_status")?;
                    let status = if dead_lettered { "dead_letter".to_string() } else { latest_status };
                    Ok(ReactorOutcomeEntry {
                        reactor_id: r.try_get("reactor_id")?,
                        status,
                        error: r.try_get("latest_error")?,
                        attempts: r.try_get("attempts")?,
                        started_at: r.try_get("started_at")?,
                        completed_at: r.try_get("completed_at")?,
                        triggering_event_ids: r.try_get("event_ids")?,
                    })
                })
                .collect()
        }

        async fn reactor_attempt_history(
            &self,
            correlation_id: &str,
        ) -> Result<Vec<ReactorAttemptEntry>> {
            let Some(cid) = parse_corr(correlation_id) else { return Ok(vec![]) };
            // Closed attempts only — an in-flight `running` row has no
            // completed_at and would render as a zero-duration attempt
            // (mirrors MemoryStore, which records history only on close).
            let sql = format!(
                "SELECT event_id, reactor_id, correlation_id, attempt, status, error,
                        started_at, completed_at
                   FROM causal_reactor_executions
                  WHERE correlation_id = $1 AND completed_at IS NOT NULL
                  ORDER BY started_at ASC
                  LIMIT {MAX_CORRELATION_ROWS}"
            );
            let rows = sqlx::query(&sql).bind(cid).fetch_all(&self.pool).await?;
            rows.iter()
                .map(|r| {
                    let completed: Option<DateTime<Utc>> = r.try_get("completed_at")?;
                    let started: DateTime<Utc> = r.try_get("started_at")?;
                    let corr: Uuid = r.try_get("correlation_id")?;
                    Ok(ReactorAttemptEntry {
                        event_id: r.try_get("event_id")?,
                        reactor_id: r.try_get("reactor_id")?,
                        correlation_id: corr.to_string(),
                        attempt: r.try_get("attempt")?,
                        status: r.try_get("status")?,
                        error: r.try_get("error")?,
                        started_at: started,
                        completed_at: completed.unwrap_or(started),
                    })
                })
                .collect()
        }

        async fn reactor_descriptions(
            &self,
            correlation_id: &str,
        ) -> Result<Vec<ReactorDescriptionEntry>> {
            let Some(cid) = parse_corr(correlation_id) else { return Ok(vec![]) };
            // Latest description per reactor (by event position).
            let rows = sqlx::query(
                "SELECT DISTINCT ON (d.reactor_id) d.reactor_id, d.description
                   FROM causal_reactor_descriptions d
                   JOIN causal_log l ON l.event_id = d.event_id
                  WHERE d.correlation_id = $1
                  ORDER BY d.reactor_id, l.position DESC",
            )
            .bind(cid)
            .fetch_all(&self.pool)
            .await?;
            rows.iter()
                .map(|r| {
                    Ok(ReactorDescriptionEntry {
                        reactor_id: r.try_get("reactor_id")?,
                        description: r.try_get("description")?,
                    })
                })
                .collect()
        }

        async fn reactor_description_snapshots(
            &self,
            correlation_id: &str,
        ) -> Result<Vec<ReactorDescriptionSnapshotEntry>> {
            let Some(cid) = parse_corr(correlation_id) else { return Ok(vec![]) };
            let sql = format!(
                "SELECT l.position AS seq, d.event_id, d.reactor_id, d.description
                   FROM causal_reactor_descriptions d
                   JOIN causal_log l ON l.event_id = d.event_id
                  WHERE d.correlation_id = $1
                  ORDER BY l.position ASC
                  LIMIT {MAX_CORRELATION_ROWS}"
            );
            let rows = sqlx::query(&sql).bind(cid).fetch_all(&self.pool).await?;
            rows.iter()
                .map(|r| {
                    Ok(ReactorDescriptionSnapshotEntry {
                        seq: r.try_get("seq")?,
                        event_id: r.try_get("event_id")?,
                        reactor_id: r.try_get("reactor_id")?,
                        description: r.try_get("description")?,
                    })
                })
                .collect()
        }

        async fn aggregate_state_timeline(
            &self,
            correlation_id: &str,
        ) -> Result<Vec<AggregateStateSnapshotEntry>> {
            let Some(cid) = parse_corr(correlation_id) else { return Ok(vec![]) };
            let sql = format!(
                "SELECT l.position AS seq, s.event_id, l.event_type, s.aggregate_key, s.state
                   FROM causal_aggregate_snapshots s
                   JOIN causal_log l ON l.event_id = s.event_id
                  WHERE s.correlation_id = $1
                  ORDER BY l.position ASC
                  LIMIT {MAX_CORRELATION_ROWS}"
            );
            let rows = sqlx::query(&sql).bind(cid).fetch_all(&self.pool).await?;
            rows.iter()
                .map(|r| {
                    Ok(AggregateStateSnapshotEntry {
                        seq: r.try_get("seq")?,
                        event_id: r.try_get("event_id")?,
                        event_type: r.try_get("event_type")?,
                        aggregate_key: r.try_get("aggregate_key")?,
                        state: r.try_get("state")?,
                    })
                })
                .collect()
        }

        async fn list_correlations(
            &self,
            search: Option<&str>,
            limit: usize,
            cursor: Option<DateTime<Utc>>,
        ) -> Result<Vec<CorrelationSummaryEntry>> {
            let rows = sqlx::query(
                "WITH summary AS (
                    SELECT correlation_id,
                           COUNT(*) AS event_count,
                           MIN(created_at) AS first_ts,
                           MAX(created_at) AS last_ts,
                           (array_agg(event_type ORDER BY position) FILTER (WHERE causation_id IS NULL))[1] AS root_event_type
                      FROM causal_log
                     WHERE correlation_id <> '00000000-0000-0000-0000-000000000000'
                     GROUP BY correlation_id
                 )
                 SELECT s.correlation_id, s.event_count, s.first_ts, s.last_ts,
                        COALESCE(s.root_event_type, '') AS root_event_type,
                        EXISTS (SELECT 1 FROM causal_reactor_executions x
                                 WHERE x.correlation_id = s.correlation_id
                                   AND x.status IN ('failed','dead_letter')) AS has_errors
                   FROM summary s
                  WHERE ($1::text IS NULL
                         OR s.correlation_id::text ILIKE '%'||$1||'%'
                         OR COALESCE(s.root_event_type,'') ILIKE '%'||$1||'%')
                    AND ($2::timestamptz IS NULL OR s.last_ts < $2)
                  ORDER BY s.last_ts DESC
                  LIMIT $3",
            )
            .bind(search)
            .bind(cursor)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
            rows.iter()
                .map(|r| {
                    let corr: Uuid = r.try_get("correlation_id")?;
                    Ok(CorrelationSummaryEntry {
                        correlation_id: corr.to_string(),
                        event_count: r.try_get("event_count")?,
                        first_ts: r.try_get("first_ts")?,
                        last_ts: r.try_get("last_ts")?,
                        root_event_type: r.try_get("root_event_type")?,
                        has_errors: r.try_get("has_errors")?,
                    })
                })
                .collect()
        }

        async fn reactor_dependencies(&self) -> Result<Vec<ReactorDependencyEntry>> {
            // Inputs: event types that triggered each reactor (executions ⋈ log).
            let input_rows = sqlx::query(
                "SELECT DISTINCT x.reactor_id, l.event_type
                   FROM causal_reactor_executions x
                   JOIN causal_log l ON l.event_id = x.event_id",
            )
            .fetch_all(&self.pool)
            .await?;
            // Outputs: event types each reactor produced (log.metadata.reactor_id).
            let output_rows = sqlx::query(
                "SELECT DISTINCT metadata->>'reactor_id' AS reactor_id, event_type
                   FROM causal_log
                  WHERE metadata->>'reactor_id' IS NOT NULL",
            )
            .fetch_all(&self.pool)
            .await?;

            use std::collections::{BTreeMap, BTreeSet};
            let mut inputs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let mut outputs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for r in &input_rows {
                let rid: String = r.try_get("reactor_id")?;
                inputs.entry(rid).or_default().insert(r.try_get("event_type")?);
            }
            for r in &output_rows {
                let rid: String = r.try_get("reactor_id")?;
                outputs.entry(rid).or_default().insert(r.try_get("event_type")?);
            }
            let mut ids: BTreeSet<String> = BTreeSet::new();
            ids.extend(inputs.keys().cloned());
            ids.extend(outputs.keys().cloned());
            Ok(ids
                .into_iter()
                .map(|reactor_id| {
                    let input_event_types =
                        inputs.get(&reactor_id).cloned().unwrap_or_default().into_iter().collect();
                    let output_event_types =
                        outputs.get(&reactor_id).cloned().unwrap_or_default().into_iter().collect();
                    ReactorDependencyEntry { reactor_id, input_event_types, output_event_types }
                })
                .collect())
        }

        async fn aggregate_lifecycle(
            &self,
            aggregate_key: &str,
            limit: usize,
        ) -> Result<Vec<AggregateLifecycleEntry>> {
            let rows = sqlx::query(
                "SELECT l.position AS seq, s.event_id, l.event_type, l.created_at AS ts,
                        s.correlation_id, s.aggregate_key, s.state
                   FROM causal_aggregate_snapshots s
                   JOIN causal_log l ON l.event_id = s.event_id
                  WHERE s.aggregate_key = $1
                  ORDER BY l.position ASC
                  LIMIT $2",
            )
            .bind(aggregate_key)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
            rows.iter()
                .map(|r| {
                    let corr: Uuid = r.try_get("correlation_id")?;
                    Ok(AggregateLifecycleEntry {
                        seq: r.try_get("seq")?,
                        event_id: r.try_get("event_id")?,
                        event_type: r.try_get("event_type")?,
                        ts: r.try_get("ts")?,
                        correlation_id: corr.to_string(),
                        aggregate_key: r.try_get("aggregate_key")?,
                        state: r.try_get("state")?,
                    })
                })
                .collect()
        }

        async fn list_aggregate_keys(&self) -> Result<Vec<String>> {
            let rows = sqlx::query(
                "SELECT DISTINCT aggregate_key FROM causal_aggregate_snapshots ORDER BY aggregate_key",
            )
            .fetch_all(&self.pool)
            .await?;
            rows.iter().map(|r| Ok(r.try_get("aggregate_key")?)).collect()
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgInspectorReadModel;
