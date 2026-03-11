use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::display::EventDisplay;
use crate::types::{AdminEvent, EventRow};

// ── Row conversion ──

fn row_to_event(r: &sqlx::postgres::PgRow) -> EventRow {
    use sqlx::Row;
    EventRow {
        id: r.get("id"),
        parent_id: r.get("parent_id"),
        seq: r.get("seq"),
        ts: r.get("ts"),
        event_type: r.get("event_type"),
        data: r.get::<serde_json::Value, _>("data"),
        run_id: r.get("run_id"),
        correlation_id: r.get("correlation_id"),
        parent_seq: r.get("parent_seq"),
        reactor_id: r.get("reactor_id"),
    }
}

fn rows_to_admin_events(
    rows: impl IntoIterator<Item = sqlx::postgres::PgRow>,
    display: &dyn EventDisplay,
) -> Vec<AdminEvent> {
    rows.into_iter()
        .map(|r| AdminEvent::from_row(row_to_event(&r), display))
        .collect()
}

// ── Queries ──

/// Single event lookup by sequence number.
pub async fn get_event_by_seq(
    pool: &PgPool,
    seq: i64,
    display: &dyn EventDisplay,
) -> anyhow::Result<Option<AdminEvent>> {
    let row = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               run_id, correlation_id, parent_seq, reactor_id
        FROM events
        WHERE seq = $1
        "#,
    )
    .bind(seq)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| AdminEvent::from_row(row_to_event(&r), display)))
}

/// Fetch events starting from a given seq (for subscription catch-up).
pub async fn get_events_from_seq(
    pool: &PgPool,
    start_seq: i64,
    limit: i64,
    display: &dyn EventDisplay,
) -> anyhow::Result<Vec<AdminEvent>> {
    let limit = limit.min(500);

    let rows = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               run_id, correlation_id, parent_seq, reactor_id
        FROM events
        WHERE seq >= $1
        ORDER BY seq ASC
        LIMIT $2
        "#,
    )
    .bind(start_seq)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows_to_admin_events(rows, display))
}

/// Paginated reverse-chronological event listing with optional filters.
pub async fn list_events_paginated(
    pool: &PgPool,
    search: Option<&str>,
    cursor: Option<i64>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    run_id: Option<&str>,
    limit: i64,
    display: &dyn EventDisplay,
) -> anyhow::Result<Vec<AdminEvent>> {
    let limit = limit.min(200);

    let rows = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               run_id, correlation_id, parent_seq, reactor_id
        FROM events
        WHERE ($1::bigint IS NULL OR seq < $1)
          AND ($2::timestamptz IS NULL OR ts >= $2)
          AND ($3::timestamptz IS NULL OR ts <= $3)
          AND ($4::text IS NULL
               OR payload::text ILIKE '%' || $4 || '%'
               OR event_type ILIKE '%' || $4 || '%'
               OR run_id ILIKE '%' || $4 || '%'
               OR correlation_id::text ILIKE '%' || $4 || '%')
          AND ($6::text IS NULL OR run_id = $6)
        ORDER BY seq DESC
        LIMIT $5
        "#,
    )
    .bind(cursor)
    .bind(from)
    .bind(to)
    .bind(search)
    .bind(limit)
    .bind(run_id)
    .fetch_all(pool)
    .await?;

    Ok(rows_to_admin_events(rows, display))
}

/// Get all events sharing the same correlation_id as the given event.
pub async fn causal_tree(
    pool: &PgPool,
    seq: i64,
    display: &dyn EventDisplay,
) -> anyhow::Result<(Vec<AdminEvent>, i64)> {
    let rows = sqlx::query(
        r#"
        SELECT e.seq, e.ts, e.event_type, e.payload AS data, e.id, e.parent_id,
               e.run_id, e.correlation_id, e.parent_seq, e.reactor_id
        FROM events e
        WHERE e.correlation_id = (SELECT correlation_id FROM events WHERE seq = $1)
          AND e.correlation_id IS NOT NULL
        ORDER BY e.seq
        "#,
    )
    .bind(seq)
    .fetch_all(pool)
    .await?;

    use sqlx::Row;
    let root_seq = rows
        .iter()
        .find(|r| r.get::<Option<Uuid>, _>("parent_id").is_none())
        .map(|r| r.get::<i64, _>("seq"))
        .unwrap_or(seq);

    Ok((rows_to_admin_events(rows, display), root_seq))
}

/// Get all events for a run_id, ordered by seq ascending.
pub async fn causal_flow(
    pool: &PgPool,
    run_id: &str,
    display: &dyn EventDisplay,
) -> anyhow::Result<Vec<AdminEvent>> {
    let rows = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               run_id, correlation_id, parent_seq, reactor_id
        FROM events
        WHERE run_id = $1
        ORDER BY seq ASC
        "#,
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;

    Ok(rows_to_admin_events(rows, display))
}

// ── Reactor queries ──

/// Reactor log row from `causal_handler_logs`.
pub struct ReactorLogRow {
    pub event_id: Uuid,
    pub reactor_id: String,
    pub level: String,
    pub message: String,
    pub data: Option<serde_json::Value>,
    pub logged_at: DateTime<Utc>,
}

/// Fetch reactor logs for a specific event + reactor.
pub async fn handler_logs(
    pool: &PgPool,
    event_id: &Uuid,
    reactor_id: &str,
) -> anyhow::Result<Vec<ReactorLogRow>> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, String, Option<serde_json::Value>, DateTime<Utc>)>(
        "SELECT event_id, reactor_id, level, message, data, logged_at \
         FROM causal_handler_logs \
         WHERE event_id = $1 AND reactor_id = $2 \
         ORDER BY id",
    )
    .bind(event_id)
    .bind(reactor_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(event_id, reactor_id, level, message, data, logged_at)| ReactorLogRow {
            event_id,
            reactor_id,
            level,
            message,
            data,
            logged_at,
        })
        .collect())
}

/// Fetch all reactor logs for a run (by correlation_id).
pub async fn handler_logs_by_run(
    pool: &PgPool,
    run_id: &str,
) -> anyhow::Result<Vec<ReactorLogRow>> {
    let correlation_id = Uuid::parse_str(run_id)
        .map_err(|e| anyhow::anyhow!("Invalid run_id as UUID: {e}"))?;

    let rows = sqlx::query_as::<_, (Uuid, String, String, String, Option<serde_json::Value>, DateTime<Utc>)>(
        "SELECT event_id, reactor_id, level, message, data, logged_at \
         FROM causal_handler_logs \
         WHERE correlation_id = $1 \
         ORDER BY id",
    )
    .bind(correlation_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(event_id, reactor_id, level, message, data, logged_at)| ReactorLogRow {
            event_id,
            reactor_id,
            level,
            message,
            data,
            logged_at,
        })
        .collect())
}

/// Aggregated reactor outcome row from `causal_effect_executions`.
pub struct ReactorOutcomeRow {
    pub reactor_id: String,
    pub status: String,
    pub error: Option<String>,
    pub attempts: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub triggering_event_ids: Vec<String>,
}

/// Fetch aggregated reactor execution outcomes for a run.
pub async fn handler_outcomes(
    pool: &PgPool,
    run_id: &str,
) -> anyhow::Result<Vec<ReactorOutcomeRow>> {
    let correlation_id = Uuid::parse_str(run_id)
        .map_err(|e| anyhow::anyhow!("Invalid run_id as UUID: {e}"))?;

    let rows = sqlx::query_as::<_, (String, String, Option<String>, i64, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<Vec<String>>)>(
        "SELECT reactor_id, \
                CASE WHEN bool_or(status = 'error') THEN 'error' \
                     WHEN bool_or(status = 'running') THEN 'running' \
                     WHEN bool_or(status = 'pending') AND bool_or(status = 'completed') THEN 'running' \
                     WHEN bool_or(status = 'pending') THEN 'pending' \
                     ELSE 'completed' END AS status, \
                string_agg(DISTINCT error, '; ') FILTER (WHERE error IS NOT NULL) AS error, \
                COALESCE(SUM(attempts), 0) AS attempts, \
                MIN(created_at) AS started_at, \
                MAX(updated_at) FILTER (WHERE status = 'completed') AS completed_at, \
                array_agg(DISTINCT event_id::text) AS triggering_event_ids \
         FROM causal_effect_executions \
         WHERE correlation_id = $1 \
         GROUP BY reactor_id",
    )
    .bind(correlation_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(reactor_id, status, error, attempts, started_at, completed_at, triggering_event_ids)| ReactorOutcomeRow {
            reactor_id,
            status,
            error,
            attempts,
            started_at,
            completed_at,
            triggering_event_ids: triggering_event_ids.unwrap_or_default(),
        })
        .collect())
}

/// Reactor description row from `causal_reactor_descriptions`.
pub struct ReactorDescriptionRow {
    pub reactor_id: String,
    pub description: serde_json::Value,
}

/// Fetch reactor describe() blocks for a run.
pub async fn reactor_descriptions(
    pool: &PgPool,
    run_id: &str,
) -> anyhow::Result<Vec<ReactorDescriptionRow>> {
    let correlation_id = Uuid::parse_str(run_id)
        .map_err(|e| anyhow::anyhow!("Invalid run_id as UUID: {e}"))?;

    let rows = sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT reactor_id, description \
         FROM causal_reactor_descriptions \
         WHERE correlation_id = $1",
    )
    .bind(correlation_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(reactor_id, description)| ReactorDescriptionRow {
            reactor_id,
            description,
        })
        .collect())
}
