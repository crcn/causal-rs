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

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use causal::types::{LogCursor, EventData, StreamRevision, StreamState};
use causal::EventLogBackend;
use causal_replay::{PgEventLogBackend, ADVISORY_LOCK_CLASS, ADVISORY_LOCK_OBJID};
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

/// Each test isolates its rows by using a unique workflow_id and
/// filtering on it. Avoids TRUNCATE (which would race other tests).
fn make_event(workflow_id: Uuid, event_type: &str) -> EventData {
    EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id,
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
    let workflow = Uuid::new_v4();
    let mut event = make_event(workflow, "test:c1");
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
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn append_to_stream_enforces_occ_c6() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let workflow = Uuid::new_v4();
    let stream_id = Uuid::new_v4();

    // Initial append at NoStream → lands at revision 0.
    let r1 = backend
        .append_to_stream(
            "order",
            stream_id,
            StreamState::NoStream,
            vec![make_event(workflow, "test:order_placed")],
        )
        .await?;
    assert_eq!(r1.revision, StreamRevision::ZERO);

    // Stale expected (still 0) → conflict.
    let stale = backend
        .append_to_stream(
            "order",
            stream_id,
            StreamState::NoStream,
            vec![make_event(workflow, "test:order_updated")],
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
            vec![make_event(workflow, "test:order_updated")],
        )
        .await?;
    assert_eq!(r2.revision, StreamRevision::from_raw(1));

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn read_all_returns_events_in_position_order() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let workflow = Uuid::new_v4();
    let mut positions = Vec::new();
    for i in 0..5 {
        let r = causal::append_event(&backend, make_event(workflow, &format!("test:n{}", i)))
            .await?;
        positions.push(r.position);
    }

    // Capture the position BEFORE the first event — read_all is exclusive.
    let before = LogCursor::from_raw(positions[0].raw() - 1);
    let loaded = backend.read_all(before, 100).await?;

    let our_events: Vec<_> = loaded
        .iter()
        .filter(|e| e.workflow_id == workflow)
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
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}

/// Regression for the read_all gap-visibility bug: `position` is a
/// BIGSERIAL assigned at INSERT, so without serialization a tailer can
/// read+checkpoint position 11 while position 10's transaction is still
/// in flight — losing 10 forever once it commits. The fix serializes
/// every append behind `pg_advisory_xact_lock(ADVISORY_LOCK_CLASS,
/// ADVISORY_LOCK_OBJID)`, making the pre-fix interleaving impossible.
/// This test proves the serialization deterministically: while a raw
/// transaction holds the lock, `append_to_stream` must queue behind it
/// and complete only after the holder releases.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn append_queues_behind_the_advisory_lock() -> Result<()> {
    let pool = connect_local().await;
    let backend = Arc::new(PgEventLogBackend::new(pool.clone()));

    // Hold the append-serialization lock from a raw transaction —
    // standing in for an in-flight append that has been assigned a
    // position but not yet committed.
    let mut blocker = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(ADVISORY_LOCK_CLASS)
        .bind(ADVISORY_LOCK_OBJID)
        .execute(&mut *blocker)
        .await?;

    // A concurrent append must now block on the same lock.
    let workflow = Uuid::new_v4();
    let event = make_event(workflow, "test:lock_queue");
    let b2 = Arc::clone(&backend);
    let pending = tokio::spawn(async move { causal::append_event(&*b2, event).await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !pending.is_finished(),
        "append must queue behind the advisory lock while a writer txn is in flight"
    );

    // Release the lock (rollback the holder) — the append completes.
    blocker.rollback().await?;
    let result = pending.await??;
    assert!(result.position > LogCursor::ZERO, "queued append landed after release");

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}

/// Two concurrent `append_to_stream` calls (distinct streams) both
/// succeed under the advisory lock — serialization, not failure — and
/// `read_all` returns both events in commit order (strictly monotonic
/// positions, no event hidden behind the other's cursor).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn concurrent_appends_serialize_and_both_land() -> Result<()> {
    let pool = connect_local().await;
    let backend = Arc::new(PgEventLogBackend::new(pool.clone()));

    let workflow = Uuid::new_v4();
    let before = backend.latest_position().await?;

    let spawn_append = |stream_id: Uuid, event: EventData| {
        let b = Arc::clone(&backend);
        tokio::spawn(async move {
            b.append_to_stream("test_concurrent", stream_id, StreamState::NoStream, vec![event])
                .await
        })
    };
    let e1 = make_event(workflow, "test:racer_a");
    let e2 = make_event(workflow, "test:racer_b");
    let (id1, id2) = (e1.event_id, e2.event_id);
    let h1 = spawn_append(Uuid::new_v4(), e1);
    let h2 = spawn_append(Uuid::new_v4(), e2);

    // Both succeed — the lock serializes, it never rejects.
    let r1 = h1.await??;
    let r2 = h2.await??;
    assert_ne!(r1.position, r2.position, "distinct appends get distinct positions");

    // Both are visible via read_all, in strictly monotonic position order.
    let visible = backend.read_all(before, 10_000).await?;
    let ours: Vec<_> = visible
        .iter()
        .filter(|e| e.workflow_id == workflow)
        .collect();
    assert_eq!(ours.len(), 2, "both concurrent appends must be tailable");
    assert!(
        ours[0].position < ours[1].position,
        "read_all returns the racers in position (== commit) order"
    );
    assert_eq!(
        [ours[0].event_id, ours[1].event_id].iter().collect::<std::collections::HashSet<_>>(),
        [id1, id2].iter().collect::<std::collections::HashSet<_>>(),
        "exactly the two racer events are visible"
    );

    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}

/// `latest_position()` is a FROZEN high-water mark under concurrent
/// appends: once a reader observes `latest_position() == p`, no event
/// at position <= p may appear later. Pre-fix, an in-flight transaction
/// holding a position below an already-committed one violates this; the
/// advisory lock makes commit order == position order, so the set of
/// events at or below any observed head is immutable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn latest_position_is_frozen_under_concurrent_appends() -> Result<()> {
    let pool = connect_local().await;
    let backend = Arc::new(PgEventLogBackend::new(pool.clone()));

    let workflow = Uuid::new_v4();

    // Our events visible at or below position `p`, by raw SQL (scoped to
    // this test's workflow so other writers on a shared DB can't
    // perturb the assertion).
    async fn ours_at_or_below(pool: &PgPool, workflow: Uuid, p: i64) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT event_id FROM causal_log
              WHERE correlation_id = $1 AND position <= $2
              ORDER BY position",
        )
        .bind(workflow)
        .bind(p)
        .fetch_all(pool)
        .await?)
    }

    // 8 concurrent appenders × 10 events each, distinct streams.
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let b = Arc::clone(&backend);
            let stream_id = Uuid::new_v4();
            let events: Vec<EventData> =
                (0..10).map(|_| make_event(workflow, "test:hwm")).collect();
            tokio::spawn(async move {
                let mut expected = StreamState::NoStream;
                for event in events {
                    let r = b
                        .append_to_stream("test_hwm", stream_id, expected, vec![event])
                        .await?;
                    expected = StreamState::StreamRevision(r.revision.raw());
                }
                Ok::<_, anyhow::Error>(())
            })
        })
        .collect();

    // While the appenders run, repeatedly snapshot (head, visible-set).
    let mut samples: Vec<(i64, Vec<Uuid>)> = Vec::new();
    while !handles.iter().all(|h| h.is_finished()) {
        let head = backend.latest_position().await?.raw() as i64;
        let seen = ours_at_or_below(&pool, workflow, head).await?;
        samples.push((head, seen));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for h in handles {
        h.await??;
    }

    // After everything committed, every snapshot must still hold: no
    // event materialized at or below a previously observed head.
    for (head, seen_then) in &samples {
        let seen_now = ours_at_or_below(&pool, workflow, *head).await?;
        assert_eq!(
            &seen_now, seen_then,
            "events appeared at or below an observed latest_position ({head}) \
             after the fact — latest_position overtook an in-flight append"
        );
    }

    sqlx::query("DELETE FROM causal_log WHERE workflow_id = $1")
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn read_stream_partitions_by_aggregate() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let workflow = Uuid::new_v4();
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
        let event = make_event(workflow, "test:stream_event");
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

    sqlx::query("DELETE FROM causal_log WHERE workflow_id = $1")
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn latest_position_reports_max() -> Result<()> {
    let pool = connect_local().await;
    let backend = PgEventLogBackend::new(pool.clone());

    let workflow = Uuid::new_v4();
    let r = causal::append_event(&backend, make_event(workflow, "test:latest"))
        .await?;

    let latest = backend.latest_position().await?;
    assert!(
        latest >= r.position,
        "latest_position must be >= the position we just wrote (other concurrent writers may push it higher)"
    );

    sqlx::query("DELETE FROM causal_log WHERE workflow_id = $1")
        .bind(workflow)
        .execute(&pool)
        .await?;
    Ok(())
}
