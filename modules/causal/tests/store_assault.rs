//! Adversarial tests for `MemoryStore`'s indexed hot paths.
//!
//! The 2026-06-12 scale pass replaced O(N) front-scans with a positional
//! binary search (`read_all`) and derived indices (`append_to_stream`,
//! `read_stream`). Indices are exactly the kind of change that stays
//! green on happy paths and rots at the edges, so this file attacks:
//!
//! - agreement with a naive reference model under a randomized workload
//! - binary-search boundary conditions
//! - the dedup branch's partial-overlap edge (tail present, head absent)
//! - multi-event batch redelivery (identical and divergent)
//! - concurrent appends + racing redeliveries (index/Vec consistency)
//! - hostile payload sizes
//! - a regression guard pinning the linear-cost fix itself

use std::sync::Arc;

use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::types::{EventData, LogCursor, RecordedEvent, StreamRevision, StreamState};
use uuid::Uuid;

fn ev(event_id: Uuid, payload: serde_json::Value) -> EventData {
    EventData {
        event_id,
        causation_id: None,
        correlation_id: Uuid::new_v4(),
        event_type: "assault:thing".into(),
        payload,
        created_at: chrono::Utc::now(),
        category: Some("assault".into()),
        stream_id: Some(Uuid::nil()),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

/// Naive reference for `read_all`: the pre-index semantics, verbatim.
fn naive_read_all(log: &[RecordedEvent], after: LogCursor, limit: usize) -> Vec<Uuid> {
    log.iter()
        .filter(|e| e.position > after)
        .take(limit)
        .map(|e| e.event_id)
        .collect()
}

/// Naive reference for `read_stream`: the pre-index semantics, verbatim.
fn naive_read_stream(
    log: &[RecordedEvent],
    category: &str,
    stream_id: Uuid,
    after: Option<StreamRevision>,
) -> Vec<Uuid> {
    log.iter()
        .filter(|e| {
            e.category == category
                && e.stream_id == stream_id
                && match after {
                    None => true,
                    Some(min) => e.revision > min,
                }
        })
        .map(|e| e.event_id)
        .collect()
}

#[tokio::test]
async fn indexed_reads_agree_with_naive_reference_under_random_workload() {
    // Randomized workload: mixed batch sizes, a handful of streams in a
    // few categories, occasional redeliveries. Then every read the
    // indexed paths serve must equal the naive filter over the raw log.
    fastrand::seed(0xCA05_0612);
    let store = MemoryStore::new();
    let categories = ["alpha", "beta", "gamma"];
    let streams: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    let mut appended_batches: Vec<(String, Uuid, Vec<EventData>)> = Vec::new();

    for _ in 0..400 {
        if !appended_batches.is_empty() && fastrand::u8(..10) == 0 {
            // Redeliver a prior batch byte-identically — must dedup,
            // never duplicate, never disturb the index.
            let (cat, sid, batch) =
                &appended_batches[fastrand::usize(..appended_batches.len())];
            store
                .append_to_stream(cat, *sid, StreamState::Any, batch.clone())
                .await
                .expect("byte-identical redelivery must succeed");
            continue;
        }
        let cat = categories[fastrand::usize(..categories.len())];
        let sid = streams[fastrand::usize(..streams.len())];
        let batch: Vec<EventData> = (0..fastrand::usize(1..4))
            .map(|i| {
                let mut e = ev(Uuid::new_v4(), serde_json::json!({ "i": i }));
                e.category = Some(cat.into());
                e.stream_id = Some(sid);
                e
            })
            .collect();
        store
            .append_to_stream(cat, sid, StreamState::Any, batch.clone())
            .await
            .expect("append failed");
        appended_batches.push((cat.to_string(), sid, batch));
    }

    let log: Vec<RecordedEvent> = store.global_log().to_vec();
    assert!(
        log.windows(2).all(|w| w[0].position < w[1].position),
        "log positions must be strictly ascending (binary-search precondition)",
    );

    // read_all: random cursors (including exact event positions — the
    // strict-exclusivity boundary) and random limits.
    for _ in 0..200 {
        let after = match fastrand::u8(..4) {
            0 => LogCursor::ZERO,
            1 => log[fastrand::usize(..log.len())].position,
            2 => log.last().unwrap().position,
            _ => LogCursor::from_raw(fastrand::u64(..log.last().unwrap().position.raw() + 10)),
        };
        let limit = fastrand::usize(..log.len() + 10);
        let got: Vec<Uuid> = store
            .read_all(after, limit)
            .await
            .unwrap()
            .iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(
            got,
            naive_read_all(&log, after, limit),
            "read_all(after={after:?}, limit={limit}) diverged from reference",
        );
    }

    // read_stream: every (category, stream) pair, every after shape.
    for cat in &categories {
        for sid in &streams {
            let stream_rows = naive_read_stream(&log, cat, *sid, None);
            for after in [
                None,
                Some(StreamRevision::from_raw(0)),
                Some(StreamRevision::from_raw(stream_rows.len().saturating_sub(1) as u64)),
                Some(StreamRevision::from_raw(stream_rows.len() as u64 + 5)),
            ] {
                let got: Vec<Uuid> = store
                    .read_stream(cat, *sid, after)
                    .await
                    .unwrap()
                    .iter()
                    .map(|e| e.event_id)
                    .collect();
                assert_eq!(
                    got,
                    naive_read_stream(&log, cat, *sid, after),
                    "read_stream({cat}, {sid}, {after:?}) diverged from reference",
                );
            }
        }
    }

    // Missing stream entirely.
    assert!(store
        .read_stream("alpha", Uuid::new_v4(), None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn read_all_boundary_edges() {
    let store = MemoryStore::new();
    // Empty log: any read is empty.
    assert!(store.read_all(LogCursor::ZERO, 10).await.unwrap().is_empty());

    for i in 0..5 {
        store
            .append_to_stream(
                "assault",
                Uuid::nil(),
                StreamState::Any,
                vec![ev(Uuid::new_v4(), serde_json::json!({ "i": i }))],
            )
            .await
            .unwrap();
    }
    let log: Vec<RecordedEvent> = store.global_log().to_vec();

    // Cursor exactly at an event's position: strictly exclusive.
    let third = log[2].position;
    let after_third = store.read_all(third, 100).await.unwrap();
    assert_eq!(after_third.len(), 2);
    assert_eq!(after_third[0].event_id, log[3].event_id);

    // Cursor at the last position: empty, not panic.
    assert!(store
        .read_all(log.last().unwrap().position, 100)
        .await
        .unwrap()
        .is_empty());

    // Cursor beyond everything: empty.
    assert!(store
        .read_all(LogCursor::from_raw(u64::MAX), 100)
        .await
        .unwrap()
        .is_empty());

    // limit 0: empty. limit > len: everything once.
    assert!(store.read_all(LogCursor::ZERO, 0).await.unwrap().is_empty());
    assert_eq!(store.read_all(LogCursor::ZERO, 9999).await.unwrap().len(), 5);
}

#[tokio::test]
async fn dedup_tail_present_head_absent_is_partial_overlap() {
    // The dedup branch keys on the batch TAIL. A batch whose tail is
    // persisted but whose head is not can only come from a torn caller
    // — it must be rejected, not collapsed into a fake redelivery.
    let store = MemoryStore::new();
    let a = ev(Uuid::new_v4(), serde_json::json!({ "n": 1 }));
    store
        .append_to_stream("assault", Uuid::nil(), StreamState::Any, vec![a.clone()])
        .await
        .unwrap();

    let head = ev(Uuid::new_v4(), serde_json::json!({ "n": 2 }));
    let err = store
        .append_to_stream("assault", Uuid::nil(), StreamState::Any, vec![head, a])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("partial-overlap"),
        "tail-present/head-absent must be a loud partial overlap, got: {err:#}",
    );
    assert_eq!(
        store.global_log().len(),
        1,
        "rejected batch must not write",
    );
}

#[tokio::test]
async fn multi_event_batch_redelivery_collapses_and_divergent_member_errors() {
    let store = MemoryStore::new();
    let batch: Vec<EventData> = (0..3)
        .map(|i| ev(Uuid::new_v4(), serde_json::json!({ "i": i })))
        .collect();
    let first = store
        .append_to_stream("assault", Uuid::nil(), StreamState::Any, batch.clone())
        .await
        .unwrap();

    // Byte-identical redelivery (re-stamped created_at): original result.
    let mut redelivery = batch.clone();
    for e in &mut redelivery {
        e.created_at = chrono::Utc::now();
    }
    let second = store
        .append_to_stream("assault", Uuid::nil(), StreamState::Any, redelivery)
        .await
        .unwrap();
    assert_eq!(first.position, second.position);
    assert_eq!(first.revision, second.revision);
    assert_eq!(store.global_log().len(), 3);

    // Divergent MIDDLE member: rejected, nothing disturbed.
    let mut poisoned = batch.clone();
    poisoned[1].payload = serde_json::json!({ "i": "tampered" });
    let err = store
        .append_to_stream("assault", Uuid::nil(), StreamState::Any, poisoned)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("divergent"), "got: {err:#}");
    let log = store.global_log();
    assert_eq!(log.len(), 3);
    assert_eq!(log[1].payload, serde_json::json!({ "i": 1 }), "original row kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_appends_and_racing_redeliveries_keep_index_consistent() {
    // Half the tasks append fresh events to a SHARED stream; the other
    // half redeliver one fixed batch over and over. Afterward: the
    // fixed batch exists exactly once, every redelivery saw the same
    // WriteResult, revisions are dense, and the indexed reads agree
    // with the naive reference.
    let store = Arc::new(MemoryStore::new());
    let sid = Uuid::nil();
    let fixed: Vec<EventData> = (0..2)
        .map(|i| ev(Uuid::new_v4(), serde_json::json!({ "fixed": i })))
        .collect();
    store
        .append_to_stream("assault", sid, StreamState::Any, fixed.clone())
        .await
        .unwrap();

    const TASKS: usize = 8;
    const PER_TASK: usize = 200;
    let mut handles = Vec::new();
    for t in 0..TASKS {
        let store = store.clone();
        let fixed = fixed.clone();
        handles.push(tokio::spawn(async move {
            let mut redelivery_results = Vec::new();
            for k in 0..PER_TASK {
                if t % 2 == 0 {
                    store
                        .append_to_stream(
                            "assault",
                            sid,
                            StreamState::Any,
                            vec![ev(Uuid::new_v4(), serde_json::json!({ "t": t, "k": k }))],
                        )
                        .await
                        .expect("fresh append failed");
                } else {
                    let r = store
                        .append_to_stream("assault", sid, StreamState::Any, fixed.clone())
                        .await
                        .expect("redelivery failed");
                    redelivery_results.push(r);
                }
            }
            redelivery_results
        }));
    }
    let mut all_redeliveries = Vec::new();
    for h in handles {
        all_redeliveries.extend(h.await.unwrap());
    }

    let log: Vec<RecordedEvent> = store.global_log().to_vec();
    let fresh = (TASKS / 2) * PER_TASK;
    assert_eq!(log.len(), 2 + fresh, "fixed batch exactly once + fresh appends");

    // Every redelivery returned the ORIGINAL coordinates.
    let original_tail = log
        .iter()
        .find(|e| e.event_id == fixed[1].event_id)
        .unwrap();
    for r in &all_redeliveries {
        assert_eq!(r.position, original_tail.position);
        assert_eq!(r.revision, original_tail.revision);
    }

    // Positions strictly ascending; the shared stream's revisions dense.
    assert!(log.windows(2).all(|w| w[0].position < w[1].position));
    let mut revisions: Vec<u64> = log
        .iter()
        .filter(|e| e.category == "assault" && e.stream_id == sid)
        .map(|e| e.revision.raw())
        .collect();
    revisions.sort_unstable();
    assert_eq!(revisions, (0..revisions.len() as u64).collect::<Vec<_>>());

    // Indexed read_stream agrees with the reference.
    let got: Vec<Uuid> = store
        .read_stream("assault", sid, None)
        .await
        .unwrap()
        .iter()
        .map(|e| e.event_id)
        .collect();
    assert_eq!(got, naive_read_stream(&log, "assault", sid, None));
}

#[tokio::test]
async fn megabyte_payload_round_trips_intact() {
    let store = MemoryStore::new();
    let blob = "x".repeat(1_000_000);
    let payload = serde_json::json!({ "blob": blob, "nested": { "deep": [1, 2, 3] } });
    let id = Uuid::new_v4();
    store
        .append_to_stream("assault", Uuid::nil(), StreamState::Any, vec![ev(id, payload.clone())])
        .await
        .unwrap();
    let rows = store.read_all(LogCursor::ZERO, 10).await.unwrap();
    assert_eq!(rows[0].payload, payload);
    let rows = store.read_stream("assault", Uuid::nil(), None).await.unwrap();
    assert_eq!(rows[0].payload, payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn append_cost_does_not_grow_with_log_size() {
    // Regression guard for the 2026-06-12 quadratic fix. Pre-fix, the
    // last 10k of 100k appends cost ~15x the first 10k (O(N) stream
    // count + dedup scans per append). Post-fix both deciles are flat.
    // The 4x threshold is far above timing noise and far below the
    // quadratic signature, so this fails loudly if anyone reintroduces
    // a per-append scan.
    let store = MemoryStore::new();
    const DECILE: usize = 10_000;

    let mut deciles = Vec::with_capacity(10);
    for _ in 0..10 {
        let t = std::time::Instant::now();
        for _ in 0..DECILE {
            store
                .append_to_stream(
                    "assault",
                    Uuid::new_v4(), // distinct streams: worst case for the old count-scan
                    StreamState::Any,
                    vec![ev(Uuid::new_v4(), serde_json::json!({}))],
                )
                .await
                .unwrap();
        }
        deciles.push(t.elapsed());
    }
    // Median-of-three on each end so one scheduler hiccup on a loaded
    // CI box can't flip the verdict either way; quadratic growth moves
    // ALL the late deciles, not one.
    let median3 = |w: &[std::time::Duration]| -> std::time::Duration {
        let mut v = w.to_vec();
        v.sort();
        v[1]
    };
    let early = median3(&deciles[0..3]);
    let late = median3(&deciles[7..10]);
    assert!(
        late < early * 4,
        "append cost grew with log size: early-decile median {early:?}, late-decile \
         median {late:?} (all deciles: {deciles:?}) — a per-append O(N) scan is back",
    );
}
