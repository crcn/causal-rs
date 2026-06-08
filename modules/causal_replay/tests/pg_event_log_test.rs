//! Integration tests for `PgEventLogBackend` (Phase 4e.2).
//!
//! Exercises the v0.3 `EventLogBackend` trait against a real Postgres
//! database. Asserts the load-bearing contracts that `MemoryStore`
//! tests already cover, but at the persistent storage layer:
//!
//! - **C1** (append idempotency on event_id) — duplicate appends collapse.
//! - **C6** (aggregate OCC) — stale `expected_version` errors out.
//! - `read_all` returns events in monotonic position order.
//! - `read_stream` partitions by `(category, stream_id)`.
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
use causal::types::{LogCursor, EventData, StreamRevision, StreamState};
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
fn make_event(correlation_id: Uuid, event_type: &str) -> EventData {
    EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        correlation_id,
        event_type: event_type.to_string(),
        payload: serde_json::json!({}),
        created_at: Utc::now(),
        category: None,
        stream_id: None,
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

    let first = causal::append_event(&backend, event.clone()).await?;

    // Same event_id, different payload — second call should be a no-op.
    event.payload = serde_json::json!({"this_should_not": "overwrite"});
    let second = causal::append_event(&backend, event).await?;

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
    let stream_id = Uuid::new_v4();

    // Initial append at NoStream → lands at revision 0.
    let r1 = backend
        .append_to_stream(
            "order",
            stream_id,
            StreamState::NoStream,
            vec![make_event(correlation, "test:order_placed")],
        )
        .await?;
    assert_eq!(r1.revision, StreamRevision::ZERO);

    // Stale expected (still 0) → conflict.
    let stale = backend
        .append_to_stream(
            "order",
            stream_id,
            StreamState::NoStream,
            vec![make_event(correlation, "test:order_updated")],
        )
        .await;
    assert!(stale.is_err(), "stale expected_version must error");
    let err = stale.unwrap_err();
    // Must be a typed ConflictError (so Engine::append can downcast + retry),
    // not just a string mentioning conflict.
    assert!(
        err.downcast_ref::<causal::event_log::ConflictError>().is_some(),
        "OCC mismatch must surface as a typed ConflictError, got: {err:?}"
    );

    // Correct expected (StreamRevision(0)) → next revision 1.
    let r2 = backend
        .append_to_stream(
            "order",
            stream_id,
            StreamState::StreamRevision(0),
            vec![make_event(correlation, "test:order_updated")],
        )
        .await?;
    assert_eq!(r2.revision, StreamRevision::from_raw(1));

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(correlation)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn read_all_returns_events_in_position_order() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let correlation = Uuid::new_v4();
    let mut positions = Vec::new();
    for i in 0..5 {
        let r = causal::append_event(&backend, make_event(correlation, &format!("test:n{}", i)))
            .await?;
        positions.push(r.position);
    }

    // Capture the position BEFORE the first event — read_all is exclusive.
    let before = LogCursor::from_raw(positions[0].raw() - 1);
    let loaded = backend.read_all(before, 100).await?;

    let our_events: Vec<_> = loaded
        .iter()
        .filter(|e| e.correlation_id == correlation)
        .collect();
    assert_eq!(our_events.len(), 5);

    let mut prev = LogCursor::ZERO;
    for e in &our_events {
        assert!(
            e.position > prev,
            "read_all must return monotonically-increasing positions"
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
async fn read_stream_partitions_by_aggregate() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let correlation = Uuid::new_v4();
    let agg_a = Uuid::new_v4();
    let agg_b = Uuid::new_v4();

    // 3 events for agg_a, 2 for agg_b — interleaved. `prev` is the
    // 0-indexed revision of the LAST event written to that stream;
    // None means "stream is empty so far".
    for (agg, prev) in [
        (agg_a, None),
        (agg_b, None),
        (agg_a, Some(0u64)),
        (agg_a, Some(1u64)),
        (agg_b, Some(0u64)),
    ] {
        let event = make_event(correlation, "test:stream_event");
        let expected = match prev {
            None => StreamState::NoStream,
            Some(r) => StreamState::StreamRevision(r),
        };
        backend
            .append_to_stream("test_aggregate", agg, expected, vec![event])
            .await?;
    }

    let stream_a = backend
        .read_stream("test_aggregate", agg_a, None)
        .await?;
    let stream_b = backend
        .read_stream("test_aggregate", agg_b, None)
        .await?;

    assert_eq!(stream_a.len(), 3);
    assert_eq!(stream_b.len(), 2);

    // 0-indexed revision ordering: events at r0, r1, r2.
    for (i, e) in stream_a.iter().enumerate() {
        assert_eq!(e.revision, StreamRevision::from_raw(i as u64));
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
    let r = causal::append_event(&backend, make_event(correlation, "test:latest"))
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
