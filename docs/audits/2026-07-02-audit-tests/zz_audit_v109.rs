//! AUDIT v109: a workflow-ROOT fact whose declared `workflow_id` field is
//! `Uuid::nil()` (trivially produced via `#[derive(Default)]` /
//! `..Default::default()`, since `Uuid::default()` IS nil) is accepted as a
//! genuine root. Every such fact — across UNRELATED runs — merges into the
//! single shared NIL workflow, which is also the workflow the engine stamps
//! on its own control-stream cancel markers.
//!
//! Correct behavior (either would pass these tests):
//!   - emit rejects a nil-valued declared workflow root loudly, OR
//!   - independent nil-rooted emits still get distinct workflows.
//!
//! Defective behavior (current): both runs share workflow nil, so
//!   - settled() on run A waits on run B's chain (violates the settle doc:
//!     "Other runs' concurrent traffic does not delay it"), and
//!   - cancel_workflow(run_B.workflow_id) collaterally fences run A.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::EngineBuilder;

fn backend(store: &Arc<MemoryStore>) -> EngineBuilder {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .allow_in_memory_effect_store_for_tests()
    .allow_in_memory_decision_store_for_tests()
}

/// Workflow ROOT: its own lifecycle, named by its own field. `Default`
/// leaves `run_id` at `Uuid::nil()` — the accidental-forgot-to-set shape.
#[causal::event(name = "audit109_run_started", subject_id = "run_id",
                workflow_id = "run_id")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RunStarted {
    run_id: Uuid,
    tag:    String,
}

// ─────────────────────────────────────────────────────────────────────
// Test 1: independent nil-rooted emits must not silently share one
// workflow (and must never share the engine's reserved control-stream
// workflow, which is also nil).
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn independent_nil_rooted_runs_must_not_merge_into_one_workflow() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store).build().await.unwrap();

    // Two INDEPENDENT runs, each forgetting to set run_id (Default = nil).
    let a = engine
        .emit(RunStarted { tag: "run-a".into(), ..Default::default() })
        .await;
    let b = engine
        .emit(RunStarted { tag: "run-b".into(), ..Default::default() })
        .await;

    match (a, b) {
        (Ok(ra), Ok(rb)) => {
            assert_ne!(
                ra.workflow_id,
                Uuid::nil(),
                "a run must not root the engine's reserved NIL workflow \
                 (the same workflow_id append_workflow_cancelled stamps on \
                 control markers)",
            );
            assert_ne!(
                ra.workflow_id, rb.workflow_id,
                "distinct emits get distinct workflow_ids (the invariant the \
                 engine's own emit_result_carries_workflow_id test asserts \
                 for undeclared facts) — nil-valued declared roots silently \
                 merge unrelated runs into ONE workflow",
            );
        }
        // A loud emit-time rejection of a nil declared root would also be
        // correct behavior.
        _ => {}
    }
    engine.shutdown().await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// Test 2: settled() coupling. Run B's reactor body blocks; run A is
// unrelated. settle(A) must not wait on B — but with both merged into
// workflow nil, drained(nil, hw) stays false while B is in flight.
// ─────────────────────────────────────────────────────────────────────

struct Blocker {
    release: Arc<tokio::sync::Semaphore>,
}
#[async_trait]
impl Reactor for Blocker {
    type Trigger = RunStarted;
    const NAME: &'static str = "audit109.blocker";
    async fn react(&self, t: &RunStarted, _ctx: Ctx<'_>) -> Result<Events> {
        if t.tag == "blocked" {
            let permit = self.release.acquire().await?;
            permit.forget();
        }
        Ok(Events::new())
    }
}

#[tokio::test]
async fn settled_must_not_couple_across_unrelated_nil_rooted_runs() {
    let store = Arc::new(MemoryStore::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let engine = backend(&store)
        .with_reactor(Blocker { release: release.clone() })
        .build()
        .await
        .unwrap();

    // Run B: nil-rooted, its reactor body blocks until released.
    let rb = engine
        .emit(RunStarted { tag: "blocked".into(), ..Default::default() })
        .await;
    // Run A: unrelated nil-rooted run.
    let ra = engine
        .emit(RunStarted { tag: "free".into(), ..Default::default() })
        .await;

    if let (Ok(_rb), Ok(ra)) = (rb, ra) {
        // Per the settle contract, run A's settle must not wait on run B's
        // blocked reactor. 5s is generous for an in-memory engine whose
        // only obligation here is one no-op reaction.
        let settled =
            tokio::time::timeout(Duration::from_secs(5), engine.settle(ra)).await;
        // Unblock B regardless, so shutdown can drain.
        release.add_permits(100);
        assert!(
            settled.is_ok(),
            "settled() for run A hung for 5s: it is coupled to UNRELATED \
             run B's blocked reactor because both nil-rooted runs merged \
             into the single shared NIL workflow",
        );
        settled.unwrap().unwrap();
    } else {
        // Loud rejection of nil roots at emit — also correct.
        release.add_permits(100);
    }
    tokio::time::timeout(Duration::from_secs(30), engine.shutdown())
        .await
        .expect("shutdown wedged")
        .unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// Control for test 2: the same topology with REAL (v4) run ids settles
// run A promptly while run B blocks — proving the hang above is the nil
// merge, not general settle coupling.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn control_distinct_run_ids_do_not_couple() {
    let store = Arc::new(MemoryStore::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let engine = backend(&store)
        .with_reactor(Blocker { release: release.clone() })
        .build()
        .await
        .unwrap();

    let _rb = engine
        .emit(RunStarted { run_id: Uuid::new_v4(), tag: "blocked".into() })
        .await
        .unwrap();
    let ra = engine
        .emit(RunStarted { run_id: Uuid::new_v4(), tag: "free".into() })
        .await
        .unwrap();

    let settled = tokio::time::timeout(Duration::from_secs(5), engine.settle(ra)).await;
    release.add_permits(100);
    assert!(
        settled.is_ok(),
        "control failed: settle coupled even with distinct v4 run ids — \
         the coupling is then NOT nil-specific",
    );
    settled.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(30), engine.shutdown())
        .await
        .expect("shutdown wedged")
        .unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// Test 3: cancel coupling. Cancelling run B (by the workflow_id its own
// EmitResult reported) must not fence unrelated run A.
// ─────────────────────────────────────────────────────────────────────

struct Counter {
    tags: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Reactor for Counter {
    type Trigger = RunStarted;
    const NAME: &'static str = "audit109.counter";
    async fn react(&self, t: &RunStarted, _ctx: Ctx<'_>) -> Result<Events> {
        self.tags.lock().push(t.tag.clone());
        Ok(Events::new())
    }
}

#[tokio::test]
async fn cancelling_one_nil_rooted_run_must_not_fence_unrelated_runs() {
    let store = Arc::new(MemoryStore::new());
    let tags: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let engine = backend(&store)
        .with_reactor(Counter { tags: tags.clone() })
        .build()
        .await
        .unwrap();

    // Run B starts, completes its reaction, then the caller cancels it
    // using the workflow_id emit reported for it.
    let rb = engine
        .emit(RunStarted { tag: "run-b".into(), ..Default::default() })
        .settled()
        .await;
    let Ok(rb) = rb else {
        // Loud rejection of nil roots at emit — correct; nothing to test.
        engine.shutdown().await.unwrap();
        return;
    };
    engine.cancel_workflow(rb.workflow_id).await.unwrap();

    // Run A is a brand-new unrelated run. Its reaction must still fire.
    let ra = engine
        .emit(RunStarted { tag: "run-a".into(), ..Default::default() })
        .settled();
    let ra = tokio::time::timeout(Duration::from_secs(10), ra)
        .await
        .expect("settled() for run A wedged");
    if ra.is_ok() {
        let seen = tags.lock().clone();
        assert!(
            seen.iter().any(|t| t == "run-a"),
            "run A's reactor never fired: cancel_workflow(run B's workflow) \
             collaterally fenced unrelated run A because both nil-rooted \
             runs share the NIL workflow (saw reactions: {seen:?})",
        );
    }
    tokio::time::timeout(Duration::from_secs(30), engine.shutdown())
        .await
        .expect("shutdown wedged")
        .unwrap();
}
