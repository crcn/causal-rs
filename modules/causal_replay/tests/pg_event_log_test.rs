//! Integration tests for `PgEventLogBackend` (Phase 4e.2).
//!
//! Exercises the v0.3 `EventLogBackend` trait against a real Postgres
//! database. Asserts the load-bearing contracts that `MemoryStore`
//! tests already cover, but at the persistent storage layer:
//!
//! - **C1** (append idempotency on event_id) — duplicate appends collapse.
//! - **C6** (aggregate OCC) — stale `expected_version` errors out.
//! - `load_from` returns events in monotonic position order.
//! - `load_stream` partitions by `(aggregate_type, aggregate_id)`.
//! - `latest_position` reports the max persisted position.
//!
//! Schema requires migration 054_causal_v03_backend_tables.sql to have
//! been applied. Tests clean up via TRUNCATE inside each test fn so the
//! local DB stays usable.
//!
//! Run with:
//!   cargo test -p causal_replay --features postgres --test pg_event_log_test -- --ignored --nocapture

#![cfg(feature = "postgres")]

use anyhow::Result;
use causal::types::{LogCursor, NewEvent, StreamVersion};
use causal::EventLogBackend;
use causal_replay::PgEventLogBackend;
use chrono::Utc;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

fn load_env() {
    // Walk up from CARGO_MANIFEST_DIR (vendor/causal_replay/) to the
    // workspace root and read .env.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".env");
    if let Ok(content) = std::fs::read_to_string(&path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if std::env::var(key.trim()).is_err() {
                    std::env::set_var(key.trim(), value.trim());
                }
            }
        }
    }
}

async fn connect_local() -> PgPool {
    load_env();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "Refusing to run integration tests against non-local Postgres: {url}"
    );
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect local postgres")
}

/// Each test isolates its rows by using a unique correlation_id and
/// filtering on it. Avoids TRUNCATE (which would race other tests).
fn make_event(correlation_id: Uuid, event_type: &str) -> NewEvent {
    NewEvent {
        event_id: Uuid::new_v4(),
        parent_id: None,
        correlation_id,
        event_type: event_type.to_string(),
        payload: serde_json::json!({}),
        created_at: Utc::now(),
        aggregate_type: None,
        aggregate_id: None,
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn append_is_idempotent_on_event_id_c1() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    // Two appends with the same event_id should collapse to one row,
    // and both calls should return the same position.
    let correlation = Uuid::new_v4();
    let mut event = make_event(correlation, "test:c1");
    let event_id = event.event_id;

    let first = backend.append(event.clone()).await?;

    // Same event_id, different payload — second call should be a no-op.
    event.payload = serde_json::json!({"this_should_not": "overwrite"});
    let second = backend.append(event).await?;

    assert_eq!(
        first.position, second.position,
        "duplicate event_id must return the same position"
    );

    // Verify the original payload was preserved (no-op update).
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM causal_log WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        payload,
        serde_json::json!({}),
        "original payload must be preserved on duplicate-event_id append"
    );

    // Cleanup.
    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(correlation)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn append_to_stream_enforces_occ_c6() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let correlation = Uuid::new_v4();
    let aggregate_id = Uuid::new_v4();

    // Initial append at version 0 → should land at version 1.
    let r1 = backend
        .append_to_stream(
            "order",
            aggregate_id,
            StreamVersion::ZERO,
            make_event(correlation, "test:order_placed"),
        )
        .await?;
    assert_eq!(r1.version, Some(StreamVersion::from_raw(1)));

    // Stale expected (still 0) → conflict.
    let stale = backend
        .append_to_stream(
            "order",
            aggregate_id,
            StreamVersion::ZERO,
            make_event(correlation, "test:order_updated"),
        )
        .await;
    assert!(stale.is_err(), "stale expected_version must error");
    let err = stale.unwrap_err().to_string();
    assert!(
        err.contains("OCC conflict"),
        "error must report OCC conflict; got: {err}"
    );

    // Correct expected (1) → next version 2.
    let r2 = backend
        .append_to_stream(
            "order",
            aggregate_id,
            StreamVersion::from_raw(1),
            make_event(correlation, "test:order_updated"),
        )
        .await?;
    assert_eq!(r2.version, Some(StreamVersion::from_raw(2)));

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(correlation)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn load_from_returns_events_in_position_order() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let correlation = Uuid::new_v4();
    let mut positions = Vec::new();
    for i in 0..5 {
        let r = backend
            .append(make_event(correlation, &format!("test:n{}", i)))
            .await?;
        positions.push(r.position);
    }

    // Capture the position BEFORE the first event — load_from is exclusive.
    let before = LogCursor::from_raw(positions[0].raw() - 1);
    let loaded = backend.load_from(before, 100).await?;

    let our_events: Vec<_> = loaded
        .iter()
        .filter(|e| e.correlation_id == correlation)
        .collect();
    assert_eq!(our_events.len(), 5);

    let mut prev = LogCursor::ZERO;
    for e in &our_events {
        assert!(
            e.position > prev,
            "load_from must return monotonically-increasing positions"
        );
        prev = e.position;
    }

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(correlation)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn load_stream_partitions_by_aggregate() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let correlation = Uuid::new_v4();
    let agg_a = Uuid::new_v4();
    let agg_b = Uuid::new_v4();

    // 3 events for agg_a, 2 for agg_b — interleaved.
    for (agg, version) in [
        (agg_a, 1u64),
        (agg_b, 1u64),
        (agg_a, 2u64),
        (agg_a, 3u64),
        (agg_b, 2u64),
    ] {
        let event = make_event(correlation, "test:stream_event");
        backend
            .append_to_stream(
                "test_aggregate",
                agg,
                StreamVersion::from_raw(version - 1),
                event,
            )
            .await?;
    }

    let stream_a = backend
        .load_stream("test_aggregate", agg_a, None)
        .await?;
    let stream_b = backend
        .load_stream("test_aggregate", agg_b, None)
        .await?;

    assert_eq!(stream_a.len(), 3);
    assert_eq!(stream_b.len(), 2);

    // Verify version ordering.
    for (i, e) in stream_a.iter().enumerate() {
        assert_eq!(e.version, Some(StreamVersion::from_raw((i + 1) as u64)));
    }

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(correlation)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn latest_position_reports_max() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let correlation = Uuid::new_v4();
    let r = backend
        .append(make_event(correlation, "test:latest"))
        .await?;

    let latest = backend.latest_position().await?;
    assert!(
        latest >= r.position,
        "latest_position must be >= the position we just wrote (other concurrent writers may push it higher)"
    );

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(correlation)
        .execute(&pool)
        .await?;
    Ok(())
}
