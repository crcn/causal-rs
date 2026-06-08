//! Backend conformance tests for the `EventLogBackend` trait.
//!
//! Each function in this module is an async test scenario taking
//! `&impl EventLogBackend`. Backend integration test files
//! (`tests/pg_event_log_test.rs`, `tests/kurrent_event_log_test.rs`,
//! and `MemoryStore`'s own tests) wrap each scenario in a
//! `#[tokio::test]` and pass a fresh backend instance.
//!
//! ## Why a shared suite
//!
//! Without conformance, backends drift silently. Concrete example:
//! the v0.4 backend trait uses 1-indexed `StreamRevision` (ZERO =
//! empty stream, first event lands at v1). Postgres and MemoryStore
//! agree on this; the Kurrent backend originally shipped with a
//! 0-indexed version map and the discrepancy was caught only when
//! someone questioned the semantics out-of-band. **A backend
//! consumer migrating across backends would see silent divergence in
//! `WriteResult.version`** — and likely only discover it when a
//! cached aggregate snapshot stopped lining up with a fresh
//! hydration.
//!
//! The suite below pins every property a backend must satisfy: every
//! impl runs every scenario; any new property is added once and
//! enforced everywhere.
//!
//! ## What the suite does NOT cover
//!
//! - Backend-specific tuning (PG pooling, Kurrent reconnection).
//! - Performance characteristics.
//! - System-event filtering (Kurrent-specific).
//! - Persistence across restarts (in-memory backends don't qualify).
//!
//! ## Isolation
//!
//! Each scenario uses fresh UUIDs for `event_id`, `correlation_id`,
//! `aggregate_id`. Two tests running in parallel against the same
//! durable backend won't collide on writes; reads are scoped by
//! correlation_id or aggregate_id rather than by clearing tables.
//!
//! This means scenarios can run against a shared backend without
//! teardown. No `TRUNCATE`, no stream deletion.

use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

use causal::types::{EventData, LogCursor, StreamRevision, StreamState};
use causal::EventLogBackend;

/// Build a stock `EventData` for tests. Caller can mutate before
/// appending if they need a non-default shape.
pub fn fresh_event(
    correlation: Uuid,
    event_type: &str,
    category: Option<&str>,
    stream_id: Option<Uuid>,
) -> EventData {
    EventData {
        event_id:        Uuid::new_v4(),
        causation_id:       None,
        correlation_id:  correlation,
        event_type:      event_type.to_string(),
        payload:         serde_json::json!({}),
        created_at:      Utc::now(),
        category:        category.map(String::from),
        stream_id,
        metadata:        serde_json::Map::new(),
        ephemeral:       None,
        persistent:      true,
    }
}

// ──────────────────────────────────────────────────────────────────
// Scenarios — `append` (non-CAS path)
// ──────────────────────────────────────────────────────────────────

/// C1: `append` is totally idempotent on `event_id`. A second call
/// with the same `event_id` returns an equivalent `WriteResult` and
/// does not create a duplicate log entry.
pub async fn append_is_idempotent_on_event_id<B: EventLogBackend>(b: &B) -> Result<()> {
    let correlation = Uuid::new_v4();
    let mut event = fresh_event(correlation, "conformance:c1", None, None);
    let event_id = event.event_id;

    let first = causal::append_event(b, event.clone()).await?;
    // Second call: same event_id, different payload — backend must
    // collapse to the first write's result.
    event.payload = serde_json::json!({"this_should_not": "overwrite"});
    let second = causal::append_event(b, event).await?;

    assert_eq!(
        first.position, second.position,
        "C1: duplicate event_id must return the same WriteResult.position"
    );

    // Verify the durable log has exactly one event with this id by
    // walking $all and counting matches.
    let count = count_event_id_in_log(b, event_id).await?;
    assert_eq!(count, 1, "C1: duplicate append must NOT create a second log entry");
    Ok(())
}

// ──────────────────────────────────────────────────────────────────
// Scenarios — `append_to_stream` (CAS path)
// ──────────────────────────────────────────────────────────────────

/// First event written to a fresh aggregate stream lands at revision
/// 0 (0-indexed, matching Kurrent). `StreamState::NoStream` means
/// "I expect no events yet."
pub async fn fresh_stream_first_event_lands_at_revision_zero<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();
    let event = fresh_event(
        Uuid::new_v4(),
        "conformance:created",
        Some(aggregate_type),
        Some(aggregate_id),
    );

    let result = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, vec![event])
        .await?;
    assert_eq!(
        result.revision,
        StreamRevision::ZERO,
        "0-indexed revision contract: fresh-stream first event lands at revision 0"
    );
    Ok(())
}

/// Sequential CAS writes produce monotonically increasing revisions
/// 0, 1, 2, ... — no gaps, no skips.
pub async fn revision_is_monotonic_within_stream<B: EventLogBackend>(b: &B) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();
    let mut expected = StreamState::NoStream;

    for n in 0..5u64 {
        let event = fresh_event(
            Uuid::new_v4(),
            "conformance:tick",
            Some(aggregate_type),
            Some(aggregate_id),
        );
        let result = b
            .append_to_stream(aggregate_type, aggregate_id, expected, vec![event])
            .await?;
        let r = result.revision;
        assert_eq!(
            r.raw(),
            n,
            "writes 0..5 must produce revisions 0, 1, 2, 3, 4"
        );
        expected = StreamState::StreamRevision(r.raw());
    }
    Ok(())
}

/// OCC: stale `expected` errors out. Writing again at NoStream after
/// the stream already has events must conflict.
pub async fn append_to_stream_rejects_stale_expected<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();

    // Write revision 0.
    let e1 = fresh_event(
        Uuid::new_v4(),
        "conformance:first",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    b.append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, vec![e1])
        .await?;

    // Try to write again at NoStream (stale — stream has events now).
    let e2 = fresh_event(
        Uuid::new_v4(),
        "conformance:stale",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let err = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, vec![e2])
        .await
        .expect_err("stale expected must error");
    // Must be a TYPED ConflictError, not just a string with "conflict" in it —
    // Engine::append's OCC retry keys on `downcast_ref::<ConflictError>()`, so a
    // bare string error would silently disable retry on this backend.
    let conflict = err.downcast_ref::<causal::event_log::ConflictError>();
    assert!(
        conflict.is_some(),
        "OCC mismatch must surface as a typed ConflictError so Engine::append \
         can downcast + retry; got: {err:?}"
    );
    Ok(())
}

/// C1 inside the CAS path: a retry of an already-landed
/// `append_to_stream` (same `event_id`) collapses to the original
/// WriteResult. Without this, naive retries against a flaky network
/// would either duplicate the event or surface a misleading OCC
/// error.
pub async fn append_to_stream_idempotent_on_event_id_retry<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();
    let event = fresh_event(
        Uuid::new_v4(),
        "conformance:retry",
        Some(aggregate_type),
        Some(aggregate_id),
    );

    let first = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, vec![event.clone()])
        .await?;
    // Same event, same expected — should collapse idempotently.
    let second = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, vec![event])
        .await?;
    assert_eq!(
        first.position, second.position,
        "C1 (CAS): idempotent retry returns the same position"
    );
    assert_eq!(
        first.revision, second.revision,
        "C1 (CAS): idempotent retry returns the same revision"
    );
    Ok(())
}

/// A multi-event batch appends ATOMICALLY: all events land at consecutive
/// revisions, the `WriteResult` describes the LAST event, and `read_stream`
/// returns the whole batch in order. This is the spine of `Engine::append`'s
/// multi-fact atomicity — exercise it directly against every backend.
pub async fn append_to_stream_batch_lands_atomically<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();
    let batch: Vec<EventData> = (0..3)
        .map(|i| {
            fresh_event(
                Uuid::new_v4(),
                &format!("conformance:batch{i}"),
                Some(aggregate_type),
                Some(aggregate_id),
            )
        })
        .collect();
    let ids: Vec<Uuid> = batch.iter().map(|e| e.event_id).collect();

    let result = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, batch)
        .await?;

    // WriteResult describes the LAST event: revision 2 (0-indexed, 3 events).
    assert_eq!(
        result.revision,
        StreamRevision::from_raw(2),
        "batch WriteResult.revision must be the last event's revision"
    );

    let events = b.read_stream(aggregate_type, aggregate_id, None).await?;
    assert_eq!(events.len(), 3, "all 3 batch events landed atomically");
    for (i, ev) in events.iter().enumerate() {
        assert_eq!(
            ev.revision,
            StreamRevision::from_raw(i as u64),
            "batch event {i} must be at consecutive revision {i}"
        );
        assert_eq!(ev.event_id, ids[i], "batch order is preserved");
    }
    Ok(())
}

/// Re-appending the IDENTICAL multi-event batch is idempotent: it collapses
/// to the original write (same `WriteResult`, no duplicates, stream doesn't
/// grow) even though the second call's `expected` is stale. Pins the
/// "detect redelivery by the batch's last event_id" contract on every backend.
pub async fn append_to_stream_batch_idempotent_on_replay<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();
    let batch: Vec<EventData> = (0..3)
        .map(|i| {
            fresh_event(
                Uuid::new_v4(),
                &format!("conformance:replay{i}"),
                Some(aggregate_type),
                Some(aggregate_id),
            )
        })
        .collect();
    let ids: Vec<Uuid> = batch.iter().map(|e| e.event_id).collect();

    let first = b
        .append_to_stream(
            aggregate_type,
            aggregate_id,
            StreamState::NoStream,
            batch.clone(),
        )
        .await?;
    // Replay the identical batch. The stream is non-empty now, so NoStream is
    // stale — but idempotency (last event_id already present) takes precedence
    // over the expected-state check.
    let second = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, batch)
        .await?;

    assert_eq!(
        first.position, second.position,
        "replaying an identical batch returns the same WriteResult.position"
    );
    assert_eq!(
        first.revision, second.revision,
        "replaying an identical batch returns the same WriteResult.revision"
    );
    for id in &ids {
        assert_eq!(
            count_event_id_in_log(b, *id).await?,
            1,
            "batch replay must not duplicate event {id}"
        );
    }
    let events = b.read_stream(aggregate_type, aggregate_id, None).await?;
    assert_eq!(events.len(), 3, "batch replay must not grow the stream");
    Ok(())
}

// ──────────────────────────────────────────────────────────────────
// Scenarios — `read_stream`
// ──────────────────────────────────────────────────────────────────

/// Streams are isolated by `(aggregate_type, aggregate_id)`.
pub async fn read_stream_partitions_by_aggregate_id<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let agg_a = Uuid::new_v4();
    let agg_b = Uuid::new_v4();

    b.append_to_stream(
        aggregate_type,
        agg_a,
        StreamState::NoStream,
        vec![fresh_event(Uuid::new_v4(), "conformance:a", Some(aggregate_type), Some(agg_a))],
    )
    .await?;
    b.append_to_stream(
        aggregate_type,
        agg_b,
        StreamState::NoStream,
        vec![fresh_event(Uuid::new_v4(), "conformance:b", Some(aggregate_type), Some(agg_b))],
    )
    .await?;

    let a = b.read_stream(aggregate_type, agg_a, None).await?;
    let other = b.read_stream(aggregate_type, agg_b, None).await?;
    assert_eq!(a.len(), 1, "aggregate A's stream has exactly its own event");
    assert_eq!(other.len(), 1, "aggregate B's stream has exactly its own event");
    assert_ne!(a[0].event_id, other[0].event_id, "no cross-pollination");
    Ok(())
}

/// `read_stream` with `after: Some(r)` returns events strictly after
/// revision r.
pub async fn read_stream_after_revision_is_strict<B: EventLogBackend>(b: &B) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();
    let mut expected = StreamState::NoStream;
    let mut ids = Vec::new();

    for _ in 0..4 {
        let event = fresh_event(
            Uuid::new_v4(),
            "conformance:multi",
            Some(aggregate_type),
            Some(aggregate_id),
        );
        ids.push(event.event_id);
        let r = b
            .append_to_stream(aggregate_type, aggregate_id, expected, vec![event])
            .await?;
        expected = StreamState::StreamRevision(r.revision.raw());
    }

    // After revision 1, we should see exactly revisions 2 and 3
    // (the 3rd and 4th events).
    let after_r1 = b
        .read_stream(aggregate_type, aggregate_id, Some(StreamRevision::from_raw(1)))
        .await?;
    assert_eq!(
        after_r1.len(),
        2,
        "after revision 1 returns events at revisions 2 and 3 — strict-after"
    );
    assert_eq!(after_r1[0].event_id, ids[2]);
    assert_eq!(after_r1[1].event_id, ids[3]);
    Ok(())
}

/// `read_stream` on a stream that has never been written returns
/// `Ok(empty Vec)` — not an error.
pub async fn read_stream_returns_empty_for_missing_stream<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let result = b
        .read_stream("conformance", Uuid::new_v4(), None)
        .await?;
    assert!(
        result.is_empty(),
        "missing stream returns empty Vec, not error"
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────
// Scenarios — `read_all` ($all)
// ──────────────────────────────────────────────────────────────────

/// `read_all(cursor, limit)` returns events whose position is
/// strictly greater than the cursor.
pub async fn read_all_returns_events_strictly_after_cursor<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let correlation = Uuid::new_v4();
    let e1 = fresh_event(correlation, "conformance:c1", None, None);
    let e1_id = e1.event_id;
    let r1 = causal::append_event(b, e1).await?;

    let e2 = fresh_event(correlation, "conformance:c2", None, None);
    let e2_id = e2.event_id;
    causal::append_event(b, e2).await?;

    // Reading $all after r1.position must NOT include e1, but MUST
    // include e2. (Filtering by correlation so we don't pick up
    // unrelated events from a shared backend.)
    let after = b.read_all(r1.position, 1000).await?;
    let mut seen_e1 = false;
    let mut seen_e2 = false;
    for ev in &after {
        if ev.correlation_id != correlation {
            continue;
        }
        if ev.event_id == e1_id {
            seen_e1 = true;
        }
        if ev.event_id == e2_id {
            seen_e2 = true;
        }
    }
    assert!(!seen_e1, "read_all(r1.position) must NOT include e1");
    assert!(seen_e2, "read_all(r1.position) must include e2");
    Ok(())
}

// ──────────────────────────────────────────────────────────────────
// Scenarios — `latest_position`
// ──────────────────────────────────────────────────────────────────

/// `latest_position()` is at least as far as any committed write.
pub async fn latest_position_reflects_committed_writes<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let event = fresh_event(Uuid::new_v4(), "conformance:latest", None, None);
    let r = causal::append_event(b, event).await?;
    let latest = b.latest_position().await?;
    assert!(
        latest >= r.position,
        "latest_position ({latest:?}) must be >= a committed write ({:?})",
        r.position,
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────

/// Walk `$all` in batches looking for `event_id`. Returns the count
/// of matching events (should be 0 or 1 under the idempotency
/// contract).
async fn count_event_id_in_log<B: EventLogBackend>(
    b: &B,
    event_id: Uuid,
) -> Result<usize> {
    let mut cursor = LogCursor::ZERO;
    let mut count = 0;
    // Cap the scan at 100k events to keep tests bounded against
    // accumulated data on durable backends.
    for _ in 0..100 {
        let batch = b.read_all(cursor, 1000).await?;
        if batch.is_empty() {
            break;
        }
        for ev in &batch {
            if ev.event_id == event_id {
                count += 1;
            }
        }
        cursor = batch.last().unwrap().position;
    }
    Ok(count)
}

/// Roll-call: every scenario in the suite. Useful for backends that
/// want to enumerate all tests in one place. Returns
/// (scenario_name, async fn) pairs.
///
/// Not currently used by the default test layout (each backend's
/// test file lists scenarios as `#[tokio::test]` fns by name) but
/// kept for future plumbing — e.g., a single `run_all_against(b)`
/// helper for ad-hoc smoke tests.
pub fn scenario_names() -> &'static [&'static str] {
    &[
        "append_is_idempotent_on_event_id",
        "fresh_stream_first_event_lands_at_revision_zero",
        "revision_is_monotonic_within_stream",
        "append_to_stream_rejects_stale_expected",
        "append_to_stream_idempotent_on_event_id_retry",
        "append_to_stream_batch_lands_atomically",
        "append_to_stream_batch_idempotent_on_replay",
        "read_stream_partitions_by_aggregate_id",
        "read_stream_after_revision_is_strict",
        "read_stream_returns_empty_for_missing_stream",
        "read_all_returns_events_strictly_after_cursor",
        "latest_position_reflects_committed_writes",
    ]
}
