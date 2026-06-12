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
//! the backend trait uses 0-indexed `StreamRevision` (the first event
//! in a fresh stream lands at revision 0; `StreamState::NoStream`
//! expects an empty stream). All three backends must agree on this;
//! a backend shipping a different indexing convention would be caught
//! by the revision scenarios below rather than discovered out-of-band.
//! **A backend consumer migrating across backends would otherwise see
//! silent divergence in `WriteResult.version`** — and likely only
//! discover it when a cached aggregate snapshot stopped lining up with
//! a fresh hydration.
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

/// C1: `append` is idempotent on `event_id` for **byte-identical**
/// redelivery. A second call with the same `event_id` and the same
/// content returns an equivalent `WriteResult` and does not create a
/// duplicate log entry. (`created_at` may differ — it's a documented
/// hint that redeliveries re-stamp.)
pub async fn append_is_idempotent_on_event_id<B: EventLogBackend>(b: &B) -> Result<()> {
    let correlation = Uuid::new_v4();
    let mut event = fresh_event(correlation, "conformance:c1", None, None);
    let event_id = event.event_id;

    let first = causal::append_event(b, event.clone()).await?;
    // Second call: identical redelivery modulo created_at (reactor
    // redelivery re-stamps it) — backend must collapse to the first
    // write's result.
    event.created_at = Utc::now();
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

/// C1b: a dedup-hit must be byte-identical. A redelivery whose
/// `payload` (or `event_type` / `correlation_id` / `causation_id`)
/// differs from the persisted row is an upstream determinism violation
/// — the producer re-decided differently on redelivery. The backend
/// MUST error loudly (never silently keep the old row while reporting
/// success) and MUST leave the original row untouched.
pub async fn divergent_redelivery_is_rejected<B: EventLogBackend>(b: &B) -> Result<()> {
    let correlation = Uuid::new_v4();
    let mut event = fresh_event(correlation, "conformance:c1b", None, None);
    let event_id = event.event_id;
    event.payload = serde_json::json!({"decision": "ship"});

    let first = causal::append_event(b, event.clone()).await?;

    // Same event_id, different payload: must be rejected.
    event.payload = serde_json::json!({"decision": "cancel"});
    let err = causal::append_event(b, event)
        .await
        .expect_err("C1b: divergent-payload redelivery must error, not collapse silently");
    anyhow::ensure!(
        err.to_string().contains("divergent"),
        "C1b: the error should name the violation (got: {err:#})",
    );

    // Original row untouched, still exactly one.
    let count = count_event_id_in_log(b, event_id).await?;
    anyhow::ensure!(count == 1, "C1b: divergent redelivery must not write");
    let row = find_event_in_log(b, event_id).await?
        .ok_or_else(|| anyhow::anyhow!("C1b: original row missing"))?;
    anyhow::ensure!(
        row.payload == serde_json::json!({"decision": "ship"}),
        "C1b: original payload must win (got: {})", row.payload,
    );
    let _ = first;
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

/// A3: an expected revision AHEAD of the head is a conflict, not a
/// write. Backends that compute `base = expected + 1` without checking
/// the actual head silently create revision holes — `StreamRevision(10)`
/// against a head of 2 lands an event at revision 11 with nothing in
/// between. Must surface a typed `ConflictError` (current = the real
/// head; `None` when the stream is empty) and leave the stream unchanged.
pub async fn expected_revision_ahead_of_head_is_rejected<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();

    // Empty stream: any concrete expectation is ahead of the head.
    let e = fresh_event(
        Uuid::new_v4(),
        "conformance:ahead",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let err = b
        .append_to_stream(
            aggregate_type,
            aggregate_id,
            StreamState::StreamRevision(5),
            vec![e],
        )
        .await
        .expect_err("expected revision on an EMPTY stream must conflict");
    let conflict = err
        .downcast_ref::<causal::event_log::ConflictError>()
        .unwrap_or_else(|| {
            panic!("ahead-of-head must be a typed ConflictError; got: {err:?}")
        });
    assert_eq!(
        conflict.current, None,
        "empty stream conflicts with current=None"
    );

    // Seed three events → head at revision 2.
    let mut expected = StreamState::NoStream;
    for _ in 0..3 {
        let e = fresh_event(
            Uuid::new_v4(),
            "conformance:seed",
            Some(aggregate_type),
            Some(aggregate_id),
        );
        let r = b
            .append_to_stream(aggregate_type, aggregate_id, expected, vec![e])
            .await?;
        expected = StreamState::StreamRevision(r.revision.raw());
    }

    // Expect revision 10 — far ahead of the head at 2.
    let e = fresh_event(
        Uuid::new_v4(),
        "conformance:ahead",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let err = b
        .append_to_stream(
            aggregate_type,
            aggregate_id,
            StreamState::StreamRevision(10),
            vec![e],
        )
        .await
        .expect_err("expected revision ahead of the head must conflict");
    let conflict = err
        .downcast_ref::<causal::event_log::ConflictError>()
        .unwrap_or_else(|| {
            panic!("ahead-of-head must be a typed ConflictError; got: {err:?}")
        });
    assert_eq!(
        conflict.current,
        Some(StreamRevision::from_raw(2)),
        "ConflictError.current must report the REAL head"
    );

    // The stream is unchanged — no event landed, no revision hole.
    let events = b.read_stream(aggregate_type, aggregate_id, None).await?;
    assert_eq!(events.len(), 3, "rejected append must not grow the stream");
    Ok(())
}

/// Crash-redelivery under CAS: a batch appended at expected revision r
/// succeeds; a FOREIGN event then lands on the same stream; the
/// identical batch (same event_ids) is re-appended at the ORIGINAL
/// expected revision r — the caller crashed before observing the `Ok`.
/// The backend must reconcile: the whole batch is already persisted, so
/// return the ORIGINAL WriteResult — not a conflict (the head moved past
/// r) and not a duplicate. This is the scenario where head validation
/// alone would break crash-redelivery (it fires before any event_id
/// uniqueness check could).
pub async fn cas_redelivery_after_foreign_write_returns_original_result<
    B: EventLogBackend,
>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();

    // Seed one event → head at revision 0, so the batch goes through
    // the StreamRevision path on every backend.
    let seed = fresh_event(
        Uuid::new_v4(),
        "conformance:seed",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let seed_id = seed.event_id;
    b.append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, vec![seed])
        .await?;

    // Batch B at expected 0 → revisions 1, 2.
    let batch: Vec<EventData> = (0..2)
        .map(|i| {
            fresh_event(
                Uuid::new_v4(),
                &format!("conformance:redeliver{i}"),
                Some(aggregate_type),
                Some(aggregate_id),
            )
        })
        .collect();
    let ids: Vec<Uuid> = batch.iter().map(|e| e.event_id).collect();
    let original = b
        .append_to_stream(
            aggregate_type,
            aggregate_id,
            StreamState::StreamRevision(0),
            batch.clone(),
        )
        .await?;

    // A foreign write advances the head past the batch.
    let foreign = fresh_event(
        Uuid::new_v4(),
        "conformance:foreign",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let foreign_id = foreign.event_id;
    b.append_to_stream(
        aggregate_type,
        aggregate_id,
        StreamState::StreamRevision(original.revision.raw()),
        vec![foreign],
    )
    .await?;

    // Crash-redelivery: identical batch, ORIGINAL expected revision.
    let redelivered = b
        .append_to_stream(
            aggregate_type,
            aggregate_id,
            StreamState::StreamRevision(0),
            batch,
        )
        .await?;
    assert_eq!(
        redelivered.position, original.position,
        "redelivery must return the ORIGINAL WriteResult.position"
    );
    assert_eq!(
        redelivered.revision, original.revision,
        "redelivery must return the ORIGINAL WriteResult.revision"
    );

    // Stream: seed + batch (exactly once) + foreign, in order.
    let events = b.read_stream(aggregate_type, aggregate_id, None).await?;
    let got: Vec<Uuid> = events.iter().map(|e| e.event_id).collect();
    assert_eq!(
        got,
        vec![seed_id, ids[0], ids[1], foreign_id],
        "stream must contain the batch exactly once plus the foreign event"
    );
    for id in &ids {
        assert_eq!(
            count_event_id_in_log(b, *id).await?,
            1,
            "redelivery must not duplicate event {id}"
        );
    }
    Ok(())
}

/// The same redelivery-after-foreign-write hazard, but on the **`Any`**
/// path — the contract every backend must honor regardless of whether it
/// has native expected-version dedup. Kurrent's best-effort EventId dedup
/// only compares against the stream head, so a foreign write between the
/// original append and the redelivery could slip a duplicate through; B3
/// closes that with a scan-then-CAS. Postgres (`UNIQUE(event_id)`) and
/// MemoryStore satisfy it inherently. (Backend-generic so all three are
/// pinned identically.)
pub async fn any_redelivery_after_foreign_write_is_idempotent<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();

    // Original Any append → revision 0.
    let original_event = fresh_event(
        Uuid::new_v4(),
        "conformance:any_redeliver",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let original_id = original_event.event_id;
    let original = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::Any, vec![original_event.clone()])
        .await?;

    // A foreign Any write advances the head past it.
    let foreign = fresh_event(
        Uuid::new_v4(),
        "conformance:foreign",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let foreign_id = foreign.event_id;
    b.append_to_stream(aggregate_type, aggregate_id, StreamState::Any, vec![foreign])
        .await?;

    // Crash-redelivery: the IDENTICAL event (same event_id), via Any,
    // now that the original is buried under the foreign write.
    let redelivered = b
        .append_to_stream(aggregate_type, aggregate_id, StreamState::Any, vec![original_event])
        .await?;
    assert_eq!(
        redelivered.position, original.position,
        "Any redelivery must return the ORIGINAL position — not a duplicate"
    );
    assert_eq!(
        redelivered.revision, original.revision,
        "Any redelivery must return the ORIGINAL revision"
    );

    // Stream: original + foreign only, original exactly once.
    let events = b.read_stream(aggregate_type, aggregate_id, None).await?;
    let got: Vec<Uuid> = events.iter().map(|e| e.event_id).collect();
    assert_eq!(
        got,
        vec![original_id, foreign_id],
        "the redelivered event must not be duplicated"
    );
    assert_eq!(
        count_event_id_in_log(b, original_id).await?,
        1,
        "exactly one copy of the redelivered event in the log"
    );
    Ok(())
}

/// Partial overlap under CAS: append `[e1, e2]` at expected revision r;
/// then attempt `[e2, e3]` at the SAME expected revision — e2 is already
/// persisted, e3 is not. That violates the all-new-or-all-already-
/// persisted precondition on `append_to_stream`: it must fail LOUDLY
/// (a plain error, NOT a typed `ConflictError` that `Engine::append`
/// would silently retry) and leave the stream unchanged. Pins the
/// full-batch verification in the shared reconcile helper — a tail-only
/// check would mistake `[e2, e3]`'s sibling shape `[e1, e2]`-tail
/// matches for clean redeliveries.
pub async fn cas_partial_overlap_batch_is_rejected_loudly<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    let aggregate_type = "conformance";
    let aggregate_id = Uuid::new_v4();

    // Seed one event → head at revision 0.
    let seed = fresh_event(
        Uuid::new_v4(),
        "conformance:seed",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    b.append_to_stream(aggregate_type, aggregate_id, StreamState::NoStream, vec![seed])
        .await?;

    // [e1, e2] at expected 0 → revisions 1, 2.
    let e1 = fresh_event(
        Uuid::new_v4(),
        "conformance:overlap1",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let e2 = fresh_event(
        Uuid::new_v4(),
        "conformance:overlap2",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let e3 = fresh_event(
        Uuid::new_v4(),
        "conformance:overlap3",
        Some(aggregate_type),
        Some(aggregate_id),
    );
    let e3_id = e3.event_id;
    b.append_to_stream(
        aggregate_type,
        aggregate_id,
        StreamState::StreamRevision(0),
        vec![e1, e2.clone()],
    )
    .await?;

    // [e2, e3] at the same expected revision: e2 already present, e3
    // new — a torn batch, not a redelivery and not a clean conflict.
    let err = b
        .append_to_stream(
            aggregate_type,
            aggregate_id,
            StreamState::StreamRevision(0),
            vec![e2, e3],
        )
        .await
        .expect_err("partially-overlapping batch must error");
    assert!(
        err.downcast_ref::<causal::event_log::ConflictError>().is_none(),
        "partial overlap is a caller BUG, not an OCC conflict — a typed \
         ConflictError would make Engine::append silently retry it; got: {err:?}"
    );
    assert!(
        format!("{err:#}").contains("overlap"),
        "error must name the partial overlap; got: {err:#}"
    );

    // Stream unchanged: seed, e1, e2 — e3 never landed.
    let events = b.read_stream(aggregate_type, aggregate_id, None).await?;
    assert_eq!(events.len(), 3, "rejected overlap must not grow the stream");
    assert!(
        events.iter().all(|e| e.event_id != e3_id),
        "no part of the rejected batch may land"
    );
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
// Scenarios — concurrency
// ──────────────────────────────────────────────────────────────────

/// Gap-visibility under concurrency: N concurrent appenders race a
/// tailer that polls `read_all` and checkpoints after every batch.
/// The tailer must eventually observe ALL N×M events exactly once, in
/// strictly monotonic position order.
///
/// This is the property the Pg backend's advisory lock exists to
/// protect: an unserialized BIGSERIAL log can expose position 11 while
/// position 10's transaction is still in flight — a tailer that reads
/// and checkpoints 11 then loses 10 forever once it commits. Any
/// backend where commit order can diverge from position order fails
/// here with a "seen N-of-200" count mismatch.
pub async fn concurrent_appends_are_tailable_without_loss<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};

    const APPENDERS: usize = 8;
    const EVENTS_PER_APPENDER: usize = 25;

    let correlation = Uuid::new_v4();
    let aggregate_type = "conformance";

    // Pre-build every batch (one distinct stream per appender) so the
    // expected event_id set is known up front.
    let stream_ids: Vec<Uuid> = (0..APPENDERS).map(|_| Uuid::new_v4()).collect();
    let batches: Vec<Vec<EventData>> = stream_ids
        .iter()
        .map(|stream_id| {
            (0..EVENTS_PER_APPENDER)
                .map(|_| {
                    fresh_event(
                        correlation,
                        "conformance:stress",
                        Some(aggregate_type),
                        Some(*stream_id),
                    )
                })
                .collect()
        })
        .collect();
    let expected_ids: HashSet<Uuid> = batches
        .iter()
        .flatten()
        .map(|e| e.event_id)
        .collect();

    // Start tailing from the current head so the scan stays bounded on
    // shared durable backends with accumulated data. Safe: none of our
    // events exist yet, so they all land strictly after this point.
    let start = b.latest_position().await?;

    let done = AtomicBool::new(false);

    // One appender per stream: CAS-chained sequential appends, so each
    // stream is internally ordered while the 8 streams race each other
    // (and the tailer) in $all.
    let appender = |i: usize| {
        let batch = &batches[i];
        let stream_id = stream_ids[i];
        async move {
            let mut expected = StreamState::NoStream;
            for event in batch {
                let r = b
                    .append_to_stream(aggregate_type, stream_id, expected, vec![event.clone()])
                    .await?;
                expected = StreamState::StreamRevision(r.revision.raw());
            }
            Ok::<_, anyhow::Error>(())
        }
    };
    let appenders = async {
        let results = tokio::join!(
            appender(0), appender(1), appender(2), appender(3),
            appender(4), appender(5), appender(6), appender(7),
        );
        done.store(true, Ordering::SeqCst);
        results
    };

    // The tailer: read a batch, verify global monotonicity, checkpoint
    // (advance the cursor) after EVERY batch — exactly what a reactor
    // runner does. Stops once the appenders are done AND a subsequent
    // poll drains nothing.
    let tailer = async {
        let mut cursor = start;
        let mut last_position = start;
        let mut seen: Vec<Uuid> = Vec::new();
        // Bounded so a buggy backend fails loudly instead of hanging.
        for _ in 0..100_000 {
            let drained = done.load(Ordering::SeqCst);
            let batch = b.read_all(cursor, 64).await?;
            if batch.is_empty() {
                if drained {
                    // `done` was set BEFORE this read started, and the
                    // read came back empty — the log is settled.
                    return Ok::<_, anyhow::Error>(seen);
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                continue;
            }
            for ev in &batch {
                // Positions are global, so strict monotonicity must
                // hold across ALL observed events — including foreign
                // ones on a shared backend.
                assert!(
                    ev.position > last_position,
                    "tailer observed non-monotonic position {:?} after {:?}",
                    ev.position, last_position,
                );
                last_position = ev.position;
                if ev.correlation_id == correlation {
                    seen.push(ev.event_id);
                }
            }
            // Checkpoint: the cursor only ever moves forward. A gap
            // that fills in later is now unreachable — which is exactly
            // what this scenario is designed to catch.
            cursor = batch.last().unwrap().position;
        }
        anyhow::bail!("tailer did not settle within the iteration bound")
    };

    let ((a0, a1, a2, a3, a4, a5, a6, a7), seen) = tokio::join!(appenders, tailer);
    for r in [a0, a1, a2, a3, a4, a5, a6, a7] {
        r?;
    }
    let seen = seen?;

    // Exactly once: no duplicates, no losses.
    let seen_set: HashSet<Uuid> = seen.iter().copied().collect();
    assert_eq!(
        seen.len(),
        seen_set.len(),
        "tailer observed duplicate events (exactly-once violated)"
    );
    assert_eq!(
        seen_set,
        expected_ids,
        "tailer must observe all {} events exactly once; saw {} — a missing \
         event means a position became visible BEHIND an already-checkpointed \
         cursor (commit order diverged from position order)",
        APPENDERS * EVENTS_PER_APPENDER,
        seen.len(),
    );
    Ok(())
}

/// A4: concurrent `Any` appends to the SAME stream must all succeed.
/// The contract says `Any` skips the expected-state check entirely, so
/// there is nothing to conflict on — a backend that reads
/// MAX(revision) and then inserts without serializing writers turns
/// that read-then-write race into spurious `ConflictError`s (or worse,
/// duplicate revisions). All N events must land, at dense revisions
/// 0..N, each exactly once.
pub async fn concurrent_any_appends_all_succeed<B: EventLogBackend>(
    b: &B,
) -> Result<()> {
    use std::collections::HashSet;

    const N: usize = 8;
    let aggregate_type = "conformance";
    let stream_id = Uuid::new_v4();
    let correlation = Uuid::new_v4();

    let events: Vec<EventData> = (0..N)
        .map(|_| {
            fresh_event(
                correlation,
                "conformance:any",
                Some(aggregate_type),
                Some(stream_id),
            )
        })
        .collect();
    let expected_ids: HashSet<Uuid> = events.iter().map(|e| e.event_id).collect();

    // N concurrent single-event `Any` appends racing on ONE stream.
    let append = |i: usize| {
        let event = events[i].clone();
        async move {
            b.append_to_stream(aggregate_type, stream_id, StreamState::Any, vec![event])
                .await
        }
    };
    let results = tokio::join!(
        append(0), append(1), append(2), append(3),
        append(4), append(5), append(6), append(7),
    );
    for r in [
        results.0, results.1, results.2, results.3,
        results.4, results.5, results.6, results.7,
    ] {
        r.map_err(|e| {
            anyhow::anyhow!(
                "concurrent Any append must succeed — `Any` skips the \
                 expected-state check, so there is nothing to conflict on; \
                 got: {e:#}"
            )
        })?;
    }

    // All N landed, exactly once, at dense revisions 0..N.
    let recorded = b.read_stream(aggregate_type, stream_id, None).await?;
    assert_eq!(recorded.len(), N, "all {N} concurrent Any appends must land");
    for (i, ev) in recorded.iter().enumerate() {
        assert_eq!(
            ev.revision,
            StreamRevision::from_raw(i as u64),
            "revisions must be dense 0..{N} — no gaps, no duplicates"
        );
    }
    let got_ids: HashSet<Uuid> = recorded.iter().map(|e| e.event_id).collect();
    assert_eq!(
        got_ids, expected_ids,
        "every event must be present exactly once"
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

/// Find one event by id in `$all` (same bounded scan as
/// [`count_event_id_in_log`]). Used by scenarios that assert the
/// persisted row's CONTENT, not just its presence.
async fn find_event_in_log<B: EventLogBackend>(
    b: &B,
    event_id: Uuid,
) -> Result<Option<causal::types::RecordedEvent>> {
    let mut cursor = LogCursor::ZERO;
    for _ in 0..100 {
        let batch = b.read_all(cursor, 1000).await?;
        if batch.is_empty() {
            break;
        }
        if let Some(ev) = batch.iter().find(|e| e.event_id == event_id) {
            return Ok(Some(ev.clone()));
        }
        cursor = batch.last().unwrap().position;
    }
    Ok(None)
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
        "divergent_redelivery_is_rejected",
        "fresh_stream_first_event_lands_at_revision_zero",
        "revision_is_monotonic_within_stream",
        "append_to_stream_rejects_stale_expected",
        "append_to_stream_idempotent_on_event_id_retry",
        "append_to_stream_batch_lands_atomically",
        "append_to_stream_batch_idempotent_on_replay",
        "expected_revision_ahead_of_head_is_rejected",
        "cas_redelivery_after_foreign_write_returns_original_result",
        "any_redelivery_after_foreign_write_is_idempotent",
        "cas_partial_overlap_batch_is_rejected_loudly",
        "read_stream_partitions_by_aggregate_id",
        "read_stream_after_revision_is_strict",
        "read_stream_returns_empty_for_missing_stream",
        "read_all_returns_events_strictly_after_cursor",
        "latest_position_reflects_committed_writes",
        "concurrent_appends_are_tailable_without_loss",
        "concurrent_any_appends_all_succeed",
    ]
}
