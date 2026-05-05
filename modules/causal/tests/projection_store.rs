//! Conformance tests for the `ProjectionStore` trait.
//!
//! These tests exercise every method on the `MemoryStore` implementation
//! and pin the contract documented on the trait. Future backend
//! implementations (Postgres, etc.) should pass the same tests when
//! parameterized over the trait.

use std::sync::Arc;

use anyhow::Result;
use causal::{LogCursor, MemoryStore, ProjectionStore};
use uuid::Uuid;

fn store() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::new())
}

fn cursor(n: u64) -> LogCursor {
    LogCursor::from_raw(n)
}

// ─── init_projection_cursor ──────────────────────────────────────────

#[tokio::test]
async fn init_creates_new_cursor_returning_true() -> Result<()> {
    let s = store();
    let created = s.init_projection_cursor("p", cursor(10)).await?;
    assert!(created, "first init should report creation");
    assert_eq!(s.get_projection_cursor("p").await?, Some(cursor(10)));
    Ok(())
}

#[tokio::test]
async fn init_is_idempotent_returning_false_on_existing() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    let created_again = s.init_projection_cursor("p", cursor(99)).await?;
    assert!(!created_again, "second init should report no-op");
    assert_eq!(
        s.get_projection_cursor("p").await?,
        Some(cursor(10)),
        "second init must NOT overwrite the existing cursor"
    );
    Ok(())
}

// ─── get_projection_cursor ───────────────────────────────────────────

#[tokio::test]
async fn get_returns_none_for_unknown_projection() -> Result<()> {
    let s = store();
    assert_eq!(s.get_projection_cursor("nope").await?, None);
    Ok(())
}

// ─── advance_projection_cursor ───────────────────────────────────────

#[tokio::test]
async fn advance_succeeds_when_expected_from_matches() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    let updated = s.advance_projection_cursor("p", cursor(10), cursor(20)).await?;
    assert!(updated);
    assert_eq!(s.get_projection_cursor("p").await?, Some(cursor(20)));
    Ok(())
}

#[tokio::test]
async fn advance_fails_cas_when_expected_from_mismatches() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    // Caller had a stale read of cursor=5; real cursor is 10.
    let updated = s.advance_projection_cursor("p", cursor(5), cursor(20)).await?;
    assert!(!updated, "CAS must fail when expected_from is stale");
    assert_eq!(
        s.get_projection_cursor("p").await?,
        Some(cursor(10)),
        "cursor must NOT advance on CAS failure"
    );
    Ok(())
}

#[tokio::test]
async fn advance_fails_when_projection_unknown() -> Result<()> {
    let s = store();
    let updated = s.advance_projection_cursor("nope", cursor(0), cursor(5)).await?;
    assert!(!updated, "advancing a non-existent projection returns false");
    Ok(())
}

#[tokio::test]
async fn advance_clears_error_state_on_success() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    s.set_projection_error("p", Some("boom"), 3).await?;

    s.advance_projection_cursor("p", cursor(10), cursor(20)).await?;

    let status = s.projection_status("p").await?.unwrap();
    assert_eq!(status.last_error, None);
    assert_eq!(status.consecutive_failures, 0);
    Ok(())
}

// ─── advance_past_failure ────────────────────────────────────────────

#[tokio::test]
async fn advance_past_failure_writes_dlq_and_advances_atomically() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    let event_id = Uuid::new_v4();

    let updated = s
        .advance_past_failure("p", cursor(10), cursor(11), event_id, "permaerr", 5)
        .await?;
    assert!(updated);

    assert_eq!(s.get_projection_cursor("p").await?, Some(cursor(11)));
    let dlq = s.list_projection_failures("p", 100).await?;
    assert_eq!(dlq.len(), 1);
    assert_eq!(dlq[0].event_id, event_id);
    assert_eq!(dlq[0].error, "permaerr");
    assert_eq!(dlq[0].attempts, 5);
    Ok(())
}

#[tokio::test]
async fn advance_past_failure_is_idempotent_on_event_id() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    let event_id = Uuid::new_v4();

    // First call: advances cursor, writes DLQ row.
    s.advance_past_failure("p", cursor(10), cursor(11), event_id, "err", 1).await?;
    // Crash-redo simulation: second call with the same event_id.
    // The CAS check fails (cursor is now 11, not 10), so cursor doesn't
    // re-advance and no duplicate DLQ row is written.
    let updated = s
        .advance_past_failure("p", cursor(10), cursor(11), event_id, "err", 1)
        .await?;
    assert!(!updated, "stale CAS must reject");

    let dlq = s.list_projection_failures("p", 100).await?;
    assert_eq!(dlq.len(), 1, "exactly one DLQ row even after duplicate call");

    // Direct duplicate write at the SAME from-position (forced by reset)
    // must also dedupe on event_id.
    s.reset_projection("p", cursor(10)).await?;
    s.advance_past_failure("p", cursor(10), cursor(11), event_id, "err", 1).await?;
    let dlq = s.list_projection_failures("p", 100).await?;
    assert_eq!(
        dlq.len(),
        1,
        "DLQ idempotency on (projection_id, event_id) — even when CAS \
         allows the cursor advance to repeat, the DLQ must not duplicate"
    );
    Ok(())
}

#[tokio::test]
async fn advance_past_failure_fails_cas_on_stale_from() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;

    let updated = s
        .advance_past_failure("p", cursor(5), cursor(11), Uuid::new_v4(), "e", 1)
        .await?;
    assert!(!updated);
    assert_eq!(s.get_projection_cursor("p").await?, Some(cursor(10)));
    let dlq = s.list_projection_failures("p", 100).await?;
    assert!(dlq.is_empty(), "no DLQ row written when CAS fails");
    Ok(())
}

#[tokio::test]
async fn advance_past_failure_clears_error_state() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    s.set_projection_error("p", Some("retrying"), 2).await?;

    s.advance_past_failure("p", cursor(10), cursor(11), Uuid::new_v4(), "final", 5)
        .await?;

    let status = s.projection_status("p").await?.unwrap();
    assert_eq!(status.last_error, None);
    assert_eq!(status.consecutive_failures, 0);
    Ok(())
}

// ─── set_projection_error ────────────────────────────────────────────

#[tokio::test]
async fn set_error_records_and_clears() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;

    s.set_projection_error("p", Some("oops"), 1).await?;
    let st = s.projection_status("p").await?.unwrap();
    assert_eq!(st.last_error.as_deref(), Some("oops"));
    assert_eq!(st.consecutive_failures, 1);
    assert!(st.last_attempt_at.is_some());

    s.set_projection_error("p", None, 0).await?;
    let st = s.projection_status("p").await?.unwrap();
    assert_eq!(st.last_error, None);
    assert_eq!(st.consecutive_failures, 0);
    Ok(())
}

#[tokio::test]
async fn set_error_on_unknown_projection_is_noop() -> Result<()> {
    let s = store();
    s.set_projection_error("nope", Some("e"), 1).await?;
    assert_eq!(s.projection_status("nope").await?, None);
    Ok(())
}

// ─── projection_status ───────────────────────────────────────────────

#[tokio::test]
async fn status_returns_none_for_unknown() -> Result<()> {
    let s = store();
    assert_eq!(s.projection_status("nope").await?, None);
    Ok(())
}

#[tokio::test]
async fn status_reflects_full_state() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(7)).await?;
    s.set_projection_error("p", Some("e"), 2).await?;
    s.set_projection_paused("p", true).await?;

    let st = s.projection_status("p").await?.unwrap();
    assert_eq!(st.projection_id, "p");
    assert_eq!(st.cursor, cursor(7));
    assert_eq!(st.paused, true);
    assert_eq!(st.last_error.as_deref(), Some("e"));
    assert_eq!(st.consecutive_failures, 2);
    assert!(st.last_attempt_at.is_some());
    Ok(())
}

// ─── list_projections ────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_empty_when_no_projections() -> Result<()> {
    let s = store();
    assert!(s.list_projections().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn list_returns_all_registered() -> Result<()> {
    let s = store();
    s.init_projection_cursor("a", cursor(1)).await?;
    s.init_projection_cursor("b", cursor(2)).await?;
    s.init_projection_cursor("c", cursor(3)).await?;

    let mut ids: Vec<_> = s.list_projections().await?
        .into_iter()
        .map(|s| s.projection_id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "b", "c"]);
    Ok(())
}

// ─── set_projection_paused ───────────────────────────────────────────

#[tokio::test]
async fn pause_and_resume_round_trips() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;

    s.set_projection_paused("p", true).await?;
    assert_eq!(s.projection_status("p").await?.unwrap().paused, true);

    s.set_projection_paused("p", false).await?;
    assert_eq!(s.projection_status("p").await?.unwrap().paused, false);
    Ok(())
}

// ─── reset_projection ────────────────────────────────────────────────

#[tokio::test]
async fn reset_overwrites_cursor_unconditionally() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    s.reset_projection("p", cursor(0)).await?;
    assert_eq!(s.get_projection_cursor("p").await?, Some(cursor(0)));
    Ok(())
}

#[tokio::test]
async fn reset_clears_error_state() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    s.set_projection_error("p", Some("e"), 5).await?;

    s.reset_projection("p", cursor(0)).await?;

    let st = s.projection_status("p").await?.unwrap();
    assert_eq!(st.last_error, None);
    assert_eq!(st.consecutive_failures, 0);
    assert_eq!(st.last_attempt_at, None);
    Ok(())
}

#[tokio::test]
async fn reset_on_unknown_projection_is_noop() -> Result<()> {
    let s = store();
    // Should not error, should not create the projection.
    s.reset_projection("nope", cursor(0)).await?;
    assert_eq!(s.get_projection_cursor("nope").await?, None);
    Ok(())
}

// ─── delete_projection ───────────────────────────────────────────────

#[tokio::test]
async fn delete_removes_cursor_and_dlq_rows() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;
    s.advance_past_failure("p", cursor(10), cursor(11), Uuid::new_v4(), "e", 1).await?;

    s.delete_projection("p").await?;

    assert_eq!(s.get_projection_cursor("p").await?, None);
    assert!(s.list_projection_failures("p", 100).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn delete_does_not_touch_other_projections() -> Result<()> {
    let s = store();
    s.init_projection_cursor("a", cursor(1)).await?;
    s.init_projection_cursor("b", cursor(2)).await?;
    s.advance_past_failure("a", cursor(1), cursor(2), Uuid::new_v4(), "e", 1).await?;
    s.advance_past_failure("b", cursor(2), cursor(3), Uuid::new_v4(), "e", 1).await?;

    s.delete_projection("a").await?;

    assert_eq!(s.get_projection_cursor("b").await?, Some(cursor(3)));
    assert_eq!(s.list_projection_failures("b", 100).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn delete_unknown_projection_is_noop() -> Result<()> {
    let s = store();
    s.delete_projection("nope").await?;
    Ok(())
}

// ─── list_projection_failures ────────────────────────────────────────

#[tokio::test]
async fn list_failures_returns_most_recent_first() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(10)).await?;

    let e1 = Uuid::new_v4();
    let e2 = Uuid::new_v4();
    let e3 = Uuid::new_v4();

    s.advance_past_failure("p", cursor(10), cursor(11), e1, "first", 1).await?;
    // Use distinct sleep to make timestamps differ even on fast CI.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    s.advance_past_failure("p", cursor(11), cursor(12), e2, "second", 1).await?;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    s.advance_past_failure("p", cursor(12), cursor(13), e3, "third", 1).await?;

    let dlq = s.list_projection_failures("p", 100).await?;
    assert_eq!(dlq.len(), 3);
    assert_eq!(dlq[0].event_id, e3, "most recent first");
    assert_eq!(dlq[1].event_id, e2);
    assert_eq!(dlq[2].event_id, e1);
    Ok(())
}

#[tokio::test]
async fn list_failures_respects_limit() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;

    for i in 0..5 {
        let event_id = Uuid::new_v4();
        s.advance_past_failure(
            "p",
            cursor(i as u64),
            cursor(i as u64 + 1),
            event_id,
            "e",
            1,
        )
        .await?;
    }

    let dlq = s.list_projection_failures("p", 2).await?;
    assert_eq!(dlq.len(), 2);
    Ok(())
}

#[tokio::test]
async fn list_failures_filters_by_projection() -> Result<()> {
    let s = store();
    s.init_projection_cursor("a", cursor(0)).await?;
    s.init_projection_cursor("b", cursor(0)).await?;

    s.advance_past_failure("a", cursor(0), cursor(1), Uuid::new_v4(), "e", 1).await?;
    s.advance_past_failure("b", cursor(0), cursor(1), Uuid::new_v4(), "e", 1).await?;

    assert_eq!(s.list_projection_failures("a", 100).await?.len(), 1);
    assert_eq!(s.list_projection_failures("b", 100).await?.len(), 1);
    assert_eq!(s.list_projection_failures("c", 100).await?.len(), 0);
    Ok(())
}

// ─── delete_projection_failure ───────────────────────────────────────

#[tokio::test]
async fn delete_failure_returns_true_on_match() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;
    let event_id = Uuid::new_v4();
    s.advance_past_failure("p", cursor(0), cursor(1), event_id, "e", 1).await?;

    let deleted = s.delete_projection_failure("p", event_id).await?;
    assert!(deleted);
    assert!(s.list_projection_failures("p", 100).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn delete_failure_returns_false_on_miss() -> Result<()> {
    let s = store();
    let deleted = s.delete_projection_failure("p", Uuid::new_v4()).await?;
    assert!(!deleted);
    Ok(())
}

#[tokio::test]
async fn delete_failure_only_removes_matching_row() -> Result<()> {
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;
    let keep = Uuid::new_v4();
    let drop = Uuid::new_v4();
    s.advance_past_failure("p", cursor(0), cursor(1), keep, "k", 1).await?;
    s.advance_past_failure("p", cursor(1), cursor(2), drop, "d", 1).await?;

    s.delete_projection_failure("p", drop).await?;

    let remaining = s.list_projection_failures("p", 100).await?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].event_id, keep);
    Ok(())
}

// ─── Concurrent CAS semantics ────────────────────────────────────────
//
// These tests are the load-bearing ones for multi-process backend
// correctness. The whole point of taking `expected_from` and returning
// `Result<bool>` is that simultaneous attempts produce one winner and
// the others see `false` — verifying that under actual concurrency.
// Sequential tests don't cover this; even if every sequential test
// passes, a buggy CAS impl could still corrupt under concurrent load.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_advance_only_one_succeeds() -> Result<()> {
    // Two tasks racing to advance the same cursor from the same expected_from
    // to different `to` positions. Exactly one should succeed; the other
    // should see CAS failure (false) and observe the winner's cursor.
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;

    let s1 = s.clone();
    let s2 = s.clone();

    let (r1, r2) = tokio::join!(
        async move { s1.advance_projection_cursor("p", cursor(0), cursor(1)).await },
        async move { s2.advance_projection_cursor("p", cursor(0), cursor(2)).await },
    );
    let r1 = r1?;
    let r2 = r2?;

    assert!(
        r1 ^ r2,
        "exactly one concurrent CAS must succeed; got r1={} r2={}",
        r1, r2
    );

    let final_cursor = s.get_projection_cursor("p").await?.unwrap();
    assert!(
        final_cursor == cursor(1) || final_cursor == cursor(2),
        "cursor must equal the winner's target; got {:?}",
        final_cursor
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_advance_high_contention() -> Result<()> {
    // 16 tasks all racing to advance from cursor(0) to cursor(N+1) where N
    // is each task's index. Exactly one should win on each generation;
    // losers re-read and retry. Verifies that under heavy contention, no
    // updates are lost and no double-applies happen.
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;

    const TASKS: u64 = 16;
    let mut handles = Vec::new();
    for i in 0..TASKS {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            // Each task tries up to 100 times to advance the cursor by 1.
            // It should eventually succeed (the loser re-reads and retries).
            let _ = i;
            for _ in 0..100 {
                let from = s.get_projection_cursor("p").await.unwrap().unwrap();
                let to = LogCursor::from_raw(from.raw() + 1);
                if s.advance_projection_cursor("p", from, to).await.unwrap() {
                    return; // succeeded
                }
                // CAS failed — another task advanced. Re-read on next loop.
            }
            panic!("task did not converge after 100 retries");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // After all tasks complete, cursor should be exactly TASKS (each task
    // contributed one successful advance, no duplicates, no losses).
    let final_cursor = s.get_projection_cursor("p").await?.unwrap();
    assert_eq!(
        final_cursor.raw(),
        TASKS,
        "exactly one advance per task should have committed"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_advance_past_failure_only_one_writes_dlq() -> Result<()> {
    // Two tasks race to advance_past_failure with the SAME event_id.
    // Exactly one CAS succeeds. Even though both attempted to write a
    // DLQ row, the loser's CAS rejection prevents its DLQ write entirely
    // (per the ordering contract: CAS check first, then DLQ write).
    // Result: one DLQ row.
    let s = store();
    s.init_projection_cursor("p", cursor(0)).await?;
    let event_id = Uuid::new_v4();

    let s1 = s.clone();
    let s2 = s.clone();

    let (r1, r2) = tokio::join!(
        async move {
            s1.advance_past_failure("p", cursor(0), cursor(1), event_id, "err-a", 3)
                .await
        },
        async move {
            s2.advance_past_failure("p", cursor(0), cursor(1), event_id, "err-b", 3)
                .await
        },
    );
    let r1 = r1?;
    let r2 = r2?;

    assert!(r1 ^ r2, "exactly one of the concurrent skips must succeed");
    let dlq = s.list_projection_failures("p", 100).await?;
    assert_eq!(
        dlq.len(),
        1,
        "exactly one DLQ row even under concurrent skip attempts; \
         loser's CAS rejection prevents DLQ write entirely (ordering contract)"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_init_only_one_creates() -> Result<()> {
    // Two tasks race to init_projection_cursor with different start
    // positions. Exactly one should report `true` (created); the other
    // sees `false` (already exists). The persisted cursor reflects the
    // winner's start position.
    let s = store();

    let s1 = s.clone();
    let s2 = s.clone();

    let (r1, r2) = tokio::join!(
        async move { s1.init_projection_cursor("p", cursor(100)).await },
        async move { s2.init_projection_cursor("p", cursor(200)).await },
    );
    let r1 = r1?;
    let r2 = r2?;

    assert!(r1 ^ r2, "exactly one init must report creation");
    let final_cursor = s.get_projection_cursor("p").await?.unwrap();
    assert!(
        final_cursor == cursor(100) || final_cursor == cursor(200),
        "cursor must equal the winner's start position; got {:?}",
        final_cursor
    );
    Ok(())
}

// ─── Cross-method invariants ─────────────────────────────────────────

#[tokio::test]
async fn full_lifecycle_roundtrip() -> Result<()> {
    // Walk through a realistic projection lifecycle:
    // init → advance several times → fail and skip → reset → advance again → delete.
    let s = store();
    assert!(s.init_projection_cursor("life", cursor(0)).await?);

    assert!(s.advance_projection_cursor("life", cursor(0), cursor(1)).await?);
    assert!(s.advance_projection_cursor("life", cursor(1), cursor(2)).await?);
    assert!(s.advance_projection_cursor("life", cursor(2), cursor(3)).await?);

    s.set_projection_error("life", Some("flap"), 1).await?;
    let bad = Uuid::new_v4();
    assert!(s
        .advance_past_failure("life", cursor(3), cursor(4), bad, "permaerr", 5)
        .await?);
    assert_eq!(s.list_projection_failures("life", 10).await?.len(), 1);

    let st = s.projection_status("life").await?.unwrap();
    assert_eq!(st.cursor, cursor(4));
    assert_eq!(st.last_error, None, "advance_past_failure cleared error state");

    // Operator notices the parked event, fixes the bug, removes the DLQ row,
    // and rewinds the cursor to replay.
    assert!(s.delete_projection_failure("life", bad).await?);
    s.reset_projection("life", cursor(3)).await?;
    assert!(s.advance_projection_cursor("life", cursor(3), cursor(4)).await?);

    s.delete_projection("life").await?;
    assert_eq!(s.get_projection_cursor("life").await?, None);
    Ok(())
}
