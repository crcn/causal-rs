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
//! - A live KurrentDB on `KURRENT_URL` (default `kurrentdb://localhost:2113?tls=false`).
//! - Docker example:
//!     docker run -d --name kurrent -p 2113:2113 \
//!       kurrentplatform/kurrentdb:latest --insecure --run-projections=All \
//!       --enable-atom-pub-over-http
//!
//! Each test scopes its assertions by a per-test workflow_id; we
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
        "kurrentdb://localhost:2113?tls=false".to_string()
    })
}

fn connect() -> KurrentEventLogBackend {
    KurrentEventLogBackend::connect(&connection_string())
        .expect("Kurrent connect failed — is the server up at KURRENT_URL?")
}

/// All tests isolate by a per-test workflow_id. Avoid stepping on
/// streams that other tests might own.
fn mk_event(
    workflow: Uuid,
    event_type: &str,
    category: Option<&str>,
    subject_id: Option<Uuid>,
) -> EventData {
    EventData {
        event_id:        Uuid::new_v4(),
        causation_id:       None,
        workflow_id:  workflow,
        event_type:      event_type.to_string(),
        payload:         serde_json::json!({ "v": 1 }),
        created_at:      Utc::now(),
        category:  category.map(String::from),
        subject_id,
        metadata:        serde_json::Map::new(),
        ephemeral:       None,
        persistent:      true,
    }
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_and_read_stream_round_trips() -> Result<()> {
    let backend = connect();

    let workflow = Uuid::new_v4();
    let agg_id = Uuid::new_v4();
    let event = mk_event(workflow, "lifecycle:run_started",
                         Some("lifecycle"), Some(agg_id));
    let event_id = event.event_id;

    let result = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, vec![event])
        .await?;
    assert_eq!(result.revision, StreamRevision::ZERO,
               "first append to fresh stream lands at revision 0 (0-indexed)");

    let stream = backend.read_stream("lifecycle", agg_id, None).await?;
    assert_eq!(stream.len(), 1, "exactly the one event we wrote");
    assert_eq!(stream[0].event_id, event_id);
    assert_eq!(stream[0].workflow_id, workflow);
    assert_eq!(stream[0].event_type, "lifecycle:run_started");
    assert_eq!(stream[0].payload, serde_json::json!({ "v": 1 }));
    Ok(())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_is_idempotent_for_duplicate_eventid_at_stream_head() -> Result<()> {
    // Back-to-back duplicate appends (same EventId, no interleaving)
    // collapse: with `StreamState::Any` Kurrent matches the EventId
    // against the events at the current stream head. Documented gap on
    // KurrentEventLogBackend::append: a retry that interleaves with
    // another append to the same stream can still duplicate — there is
    // no time window. This test pins the no-interleave behavior.
    let backend = connect();
    let workflow = Uuid::new_v4();
    let mut event = mk_event(workflow, "telemetry:ping", None, None);
    let event_id = event.event_id;

    let first = causal::append_event(&backend, event.clone()).await?;
    // Byte-identical redelivery: collapses to the original write.
    let second = causal::append_event(&backend, event.clone()).await?;
    assert_eq!(first.position, second.position,
               "duplicate event_id at stream head returns same position");

    // DIVERGENT redelivery: rejected loudly (C1b — the 0.8 silent
    // keep-the-old-row contract was replaced in the 0.10 audit).
    event.payload = serde_json::json!({"this_should_not": "overwrite"});
    let err = causal::append_event(&backend, event)
        .await
        .expect_err("divergent redelivery must be rejected");
    assert!(format!("{err:#}").contains("divergent redelivery"), "{err:#}");

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
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, vec![e0])
        .await?;
    assert_eq!(r0.revision, StreamRevision::ZERO);

    // Try to write again at expected=0 (stale) — should conflict.
    let e1 = mk_event(Uuid::new_v4(), "lifecycle:updated",
                      Some("lifecycle"), Some(agg_id));
    let err = backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, vec![e1])
        .await
        .expect_err("stale expected version must error");
    // Must be a typed ConflictError (so Engine::append can downcast + retry),
    // not just a string mentioning conflict.
    assert!(
        err.downcast_ref::<causal::event_log::ConflictError>().is_some(),
        "OCC mismatch must surface as a typed ConflictError, got: {err:?}"
    );
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
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, vec![event.clone()])
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
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, vec![event])
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
            vec![mk_event(Uuid::new_v4(), "lifecycle:a",
                     Some("lifecycle"), Some(agg_a))],
        )
        .await?;
    backend
        .append_to_stream(
            "lifecycle", agg_b, StreamState::NoStream,
            vec![mk_event(Uuid::new_v4(), "lifecycle:b",
                     Some("lifecycle"), Some(agg_b))],
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
    let workflow = Uuid::new_v4();

    let mut event = mk_event(workflow, "lifecycle:tagged",
                             Some("lifecycle"), Some(agg_id));
    event.causation_id = Some(parent);
    event.metadata.insert(
        "_run_id".to_string(),
        serde_json::Value::String("r-42".to_string()),
    );
    let event_id = event.event_id;

    backend
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, vec![event])
        .await?;

    let stream = backend.read_stream("lifecycle", agg_id, None).await?;
    let loaded = stream.iter().find(|e| e.event_id == event_id)
        .expect("event in stream");
    assert_eq!(loaded.workflow_id, workflow,
               "$correlationId was stamped and stripped back into the field");
    assert_eq!(loaded.causation_id, Some(parent),
               "$causationId was stamped and stripped back into the field");
    assert_eq!(loaded.category, "lifecycle",
               "_aggregateType was stamped and stripped back into the field");
    assert_eq!(loaded.subject_id, agg_id);
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
        .append_to_stream("lifecycle", agg_id, StreamState::NoStream, vec![event])
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

// ── Live end-to-end: reactor output streams to its own stream (bridge) ──
//
// Proves Phase 4's "bridge" slice against a REAL KurrentDB: a reactor's
// output lands in `{output_category}-{output_subject_id}`, not a shared
// `_global`. Hybrid wiring — Kurrent is the event log; MemoryStore is the
// reactor outbox + checkpoint (Kurrent is a log, not a job queue).

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct FetchRequested { id: Uuid, occurred_at: chrono::DateTime<Utc> }
impl causal::Event for FetchRequested {
    const NAME: &'static str = "requested";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<chrono::DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Fetched { id: Uuid, occurred_at: chrono::DateTime<Utc> }
impl causal::Event for Fetched {
    const NAME: &'static str = "done";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<chrono::DateTime<Utc>> { Some(self.occurred_at) }
}

struct DoFetch;
#[async_trait::async_trait]
impl causal::Reactor for DoFetch {
    type Trigger = FetchRequested;
    const NAME: &'static str = "do_fetch_reactor";
    async fn react(&self, t: &FetchRequested, ctx: causal::Ctx<'_>) -> Result<causal::Events> {
        let mut out = causal::Events::new();
        out.push(Fetched { id: t.id, occurred_at: ctx.time() });
        Ok(out)
    }
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn reactor_output_lands_in_its_own_stream_not_global() {
    use causal::{CheckpointStore, EngineBuilder, MemoryStore, ReactorCheckpoint};
    use std::sync::Arc;

    let kurrent = Arc::new(connect());
    let mem = Arc::new(MemoryStore::new());
    let engine = EngineBuilder::new(
        kurrent.clone() as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_reactor(DoFetch)
    .build().await.unwrap();

    let id = Uuid::new_v4();
    engine
        .emit(FetchRequested { id, occurred_at: Utc::now() })
        .settled()
        .await
        .unwrap();

    // The Fetched output must be in ITS OWN stream `done-{id}` (flat
    // naming: placement = the fact's NAME unless SUBJECT overrides),
    // routed by the output Event's own declarations — not `_global`.
    let out = kurrent.read_stream("done", id, None).await.unwrap();
    assert_eq!(out.len(), 1, "exactly one Fetched in done-{id}");
    assert_eq!(out[0].event_type, "done");
    assert_eq!(out[0].category, "done");
    assert_eq!(out[0].subject_id, id);
    assert!(out[0].causation_id.is_some(), "output carries the trigger as causation");

    engine.shutdown().await.unwrap();
}
