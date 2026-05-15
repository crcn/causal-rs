//! Integration tests for `KurrentEventLogBackend`.
//!
//! Exercises the `EventLogBackend` trait against a real KurrentDB
//! instance. Mirrors the PG integration test layout
//! (`pg_event_log_test.rs`).
//!
//! Run with:
//!   cargo test -p causal_replay --features kurrent \
//!     --test kurrent_event_log_test -- --ignored --nocapture
//!
//! Requires:
//! - A live KurrentDB on `KURRENT_URL` (default `esdb://localhost:2113?tls=false`).
//! - Docker example:
//!     docker run -d --name kurrent -p 2113:2113 \
//!       kurrent/kurrentdb-ce:latest --insecure --run-projections=All \
//!       --enable-atom-pub-over-http
//!
//! Each test scopes its assertions by a per-test correlation_id; we
//! don't try to truncate Kurrent between runs (Kurrent isn't really
//! "TRUNCATE-able" — stream deletion is logical). Tests must remain
//! tolerant of pre-existing data in `$all`.

#![cfg(feature = "kurrent")]

use anyhow::Result;
use causal::types::{LogCursor, EventData, StreamRevision, StreamState};
use causal::EventLogBackend;
use causal_replay::KurrentEventLogBackend;
use chrono::Utc;
use uuid::Uuid;

fn connection_string() -> String {
    std::env::var("KURRENT_URL").unwrap_or_else(|_| {
        "esdb://localhost:2113?tls=false".to_string()
    })
}

fn connect() -> KurrentEventLogBackend {
    KurrentEventLogBackend::connect(&connection_string())
        .expect("Kurrent connect failed — is the server up at KURRENT_URL?")
}

/// All tests isolate by a per-test correlation_id. Avoid stepping on
/// streams that other tests might own.
fn mk_event(
    correlation: Uuid,
    event_type: &str,
    aggregate_type: Option<&str>,
    aggregate_id: Option<Uuid>,
) -> EventData {
    EventData {
        event_id:        Uuid::new_v4(),
        causation_id:       None,
        correlation_id:  correlation,
        event_type:      event_type.to_string(),
        payload:         serde_json::json!({ "v": 1 }),
        created_at:      Utc::now(),
        aggregate_type:  aggregate_type.map(String::from),
        aggregate_id,
        metadata:        serde_json::Map::new(),
        ephemeral:       None,
        persistent:      true,
    }
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_and_read_stream_round_trips() -> Result<()> {
    let backend = connect();

    let correlation = Uuid::new_v4();
    let agg_id = Uuid::new_v4();
    let event = mk_event(correlation, "lifecycle:run_started",
                         Some("lifecycle"), Some(agg_id));
    let event_id = event.event_id;

    let result = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, event)
        .await?;
    assert_eq!(result.revision, Some(StreamRevision::ZERO),
               "first append to fresh stream lands at revision 0 (0-indexed)");

    let stream = backend.read_stream("lifecycle", agg_id, None).await?;
    assert_eq!(stream.len(), 1, "exactly the one event we wrote");
    assert_eq!(stream[0].event_id, event_id);
    assert_eq!(stream[0].correlation_id, correlation);
    assert_eq!(stream[0].event_type, "lifecycle:run_started");
    assert_eq!(stream[0].payload, serde_json::json!({ "v": 1 }));
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_is_idempotent_within_kurrent_eventid_cache() -> Result<()> {
    // Within Kurrent's ~1-min EventId cache, duplicate appends collapse.
    // Documented gap on KurrentEventLogBackend::append: post-cache
    // retries can produce duplicates — this test pins the in-window
    // behavior.
    let backend = connect();
    let correlation = Uuid::new_v4();
    let mut event = mk_event(correlation, "telemetry:ping", None, None);
    let event_id = event.event_id;

    let first = backend.append(event.clone()).await?;
    // Same event_id, different payload — Kurrent collapses the write.
    event.payload = serde_json::json!({"this_should_not": "overwrite"});
    let second = backend.append(event).await?;

    assert_eq!(first.position, second.position,
               "duplicate event_id within cache window returns same position");

    // The first payload is the one that landed.
    let stream = backend.read_stream("telemetry", Uuid::nil(), None).await;
    // We can't easily find _the_ stream "telemetry-_global" via
    // read_stream (that path is for aggregate streams). Instead, scan
    // $all for our event_id and verify the payload.
    let mut found = None;
    let mut cursor = LogCursor::ZERO;
    for _ in 0..50 {
        let batch = backend.read_all(cursor, 200).await?;
        if batch.is_empty() { break; }
        for ev in &batch {
            if ev.event_id == event_id {
                found = Some(ev.clone());
                break;
            }
            cursor = ev.position;
        }
        if found.is_some() { break; }
    }
    let _ = stream;  // unused — keep the call to verify read_stream doesn't panic on missing stream
    let found = found.expect("event must appear in $all");
    assert_eq!(found.payload, serde_json::json!({ "v": 1 }),
               "original payload preserved on duplicate-EventId append");
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_to_stream_enforces_occ() -> Result<()> {
    let backend = connect();
    let agg_id = Uuid::new_v4();

    // Write version 0.
    let e0 = mk_event(Uuid::new_v4(), "lifecycle:created",
                      Some("lifecycle"), Some(agg_id));
    let r0 = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, e0)
        .await?;
    assert_eq!(r0.revision, Some(StreamRevision::ZERO));

    // Try to write again at expected=0 (stale) — should conflict.
    let e1 = mk_event(Uuid::new_v4(), "lifecycle:updated",
                      Some("lifecycle"), Some(agg_id));
    let err = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, e1)
        .await
        .expect_err("stale expected version must error");
    let msg = format!("{err:?}");
    assert!(msg.contains("OCC conflict"),
            "error must mention OCC conflict, got: {msg}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_to_stream_retry_with_same_event_id_is_idempotent() -> Result<()> {
    // Locked Q1 decision: on WrongExpectedVersion, the backend scans
    // the conflict slice for our event_id. If found, return the
    // original WriteResult — the retry collapses.
    let backend = connect();
    let agg_id = Uuid::new_v4();

    let event = mk_event(Uuid::new_v4(), "lifecycle:created",
                         Some("lifecycle"), Some(agg_id));
    let first = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, event.clone())
        .await?;

    // Replay the exact same event at the same expected version. The
    // stream is now at revision 0 (post-write); expected=0 conflicts
    // because Kurrent sees the stream advanced. We scan the slice
    // [1, 0] — wait, that's the no-overlap case. The retry path
    // applies when expected < current; with first-write-already-done,
    // current = 0 and expected = 0 → Kurrent might not return
    // WrongExpectedVersion at all (or return it with current=0).
    // Either way, "retry of an already-landed event at the same
    // expected version" should not duplicate.
    let second = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, event)
        .await;

    match second {
        Ok(r) => {
            assert_eq!(r.position, first.position,
                       "idempotent retry returns the original position");
        }
        Err(e) => {
            // If Kurrent treats this as a strict conflict and our
            // event_id isn't in the [expected+1, current] slice,
            // we'd get an OCC error here. That would be a backend
            // bug — flag it.
            panic!("idempotent retry must not error; got: {e:?}");
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn read_stream_partitions_by_aggregate() -> Result<()> {
    let backend = connect();
    let agg_a = Uuid::new_v4();
    let agg_b = Uuid::new_v4();

    backend
        .append_to_stream(
            "lifecycle", agg_a, StreamState::NoStream,
            mk_event(Uuid::new_v4(), "lifecycle:a",
                     Some("lifecycle"), Some(agg_a)),
        )
        .await?;
    backend
        .append_to_stream(
            "lifecycle", agg_b, StreamState::NoStream,
            mk_event(Uuid::new_v4(), "lifecycle:b",
                     Some("lifecycle"), Some(agg_b)),
        )
        .await?;

    let a = backend.read_stream("lifecycle", agg_a, None).await?;
    let b = backend.read_stream("lifecycle", agg_b, None).await?;
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_eq!(a[0].event_type, "lifecycle:a");
    assert_eq!(b[0].event_type, "lifecycle:b");
    assert_ne!(a[0].event_id, b[0].event_id);
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn metadata_round_trips_with_reserved_keys_stripped() -> Result<()> {
    let backend = connect();
    let agg_id = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let correlation = Uuid::new_v4();

    let mut event = mk_event(correlation, "lifecycle:tagged",
                             Some("lifecycle"), Some(agg_id));
    event.causation_id = Some(parent);
    event.metadata.insert(
        "_run_id".to_string(),
        serde_json::Value::String("r-42".to_string()),
    );
    let event_id = event.event_id;

    backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, event)
        .await?;

    let stream = backend.read_stream("lifecycle", agg_id, None).await?;
    let loaded = stream.iter().find(|e| e.event_id == event_id)
        .expect("event in stream");
    assert_eq!(loaded.correlation_id, correlation,
               "$correlationId was stamped and stripped back into the field");
    assert_eq!(loaded.causation_id, Some(parent),
               "$causationId was stamped and stripped back into the field");
    assert_eq!(loaded.aggregate_type.as_deref(), Some("lifecycle"),
               "_aggregateType was stamped and stripped back into the field");
    assert_eq!(loaded.aggregate_id, Some(agg_id));
    assert!(loaded.persistent);
    // User metadata survives.
    assert_eq!(
        loaded.metadata.get("_run_id").and_then(|v| v.as_str()),
        Some("r-42")
    );
    // Reserved keys are NOT in the public metadata map.
    assert!(!loaded.metadata.contains_key("$correlationId"));
    assert!(!loaded.metadata.contains_key("$causationId"));
    assert!(!loaded.metadata.contains_key("_aggregateType"));
    assert!(!loaded.metadata.contains_key("_persistent"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn latest_position_reflects_a_recent_write() -> Result<()> {
    let backend = connect();
    let agg_id = Uuid::new_v4();
    let event = mk_event(Uuid::new_v4(), "lifecycle:ping",
                         Some("lifecycle"), Some(agg_id));

    let write = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, event)
        .await?;
    let latest = backend.latest_position().await?;
    assert!(latest >= write.position,
            "latest_position must be at least as far as our write");
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn read_stream_returns_empty_for_missing_stream() -> Result<()> {
    let backend = connect();
    // No writes; load a brand-new aggregate id.
    let result = backend
        .read_stream("lifecycle", Uuid::new_v4(), None)
        .await?;
    assert!(result.is_empty(), "missing stream returns empty Vec, not error");
    Ok(())
}
