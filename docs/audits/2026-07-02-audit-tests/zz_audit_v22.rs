//! Audit verification for FINDING #22: `settle()` ignores its emit-position
//! floor when a stale tracker entry exists for the workflow.
//!
//! Shape:
//!   1. Fire-and-forget emit of Ping(n=1) into workflow W. The reactor emits
//!      Pong, bumping `workflow_hw[W]` to Pong-1's position. Nothing ever
//!      removes that entry (only settle's `forget` or cap eviction do).
//!   2. Wait until the chain fully drains (reaction ran + grace period so all
//!      consumers scan past Pong-1's position).
//!   3. Emit Ping(n=2) into the SAME workflow W and await `.settled()`.
//!
//! Contract (settle docstring): the high-water is "floored at
//! `result.position` so we always wait for the trigger to be observed."
//! If the floor works, `settled()` cannot return before the reactor has
//! processed Ping(n=2) — i.e. the react counter must be 2.
//!
//! Defect: `settle` computes `hw = workflow_hw.get(&wf).unwrap_or(result.position)`
//! — the emit position is used only when the entry is ABSENT, never as a
//! max() floor — so hw is the STALE Pong-1 position, every consumer is
//! already drained to it, and settle returns while Ping(n=2) is unscanned.

use anyhow::Result;
use async_trait::async_trait;
use causal::{Ctx, EngineBuilder, Event, Events, Ordering, Reactor};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    id: Uuid,
    n: u32,
}
impl Event for Ping {
    const NAME: &'static str = "audit22_ping";
    const SUBJECT: &'static str = "audit22run";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pong {
    id: Uuid,
    n: u32,
}
impl Event for Pong {
    const NAME: &'static str = "audit22_pong";
    const SUBJECT: &'static str = "audit22run";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

/// Reacts to every Ping by emitting a Pong (which bumps the settle
/// tracker for the trigger's workflow) and counting the invocation.
struct ReactPing(Arc<AtomicU32>);

#[async_trait]
impl Reactor for ReactPing {
    type Trigger = Ping;
    const NAME: &'static str = "audit22.react.ping";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, t: &Ping, _ctx: Ctx<'_>) -> Result<Events> {
        let mut out = Events::new();
        out.push(Pong { id: t.id, n: t.n });
        self.0.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(out)
    }
}

#[tokio::test]
async fn settled_waits_for_trigger_despite_stale_tracker_entry() {
    let reacts = Arc::new(AtomicU32::new(0));
    let engine = EngineBuilder::memory()
        .with_reactor(ReactPing(reacts.clone()))
        .build()
        .await
        .unwrap();

    let w = Uuid::new_v4();

    // (1) Fire-and-forget emit into W. The reactor's Pong append bumps
    // workflow_hw[W]; no settle is called, so the entry persists.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.emit(Ping { id: w, n: 1 }).workflow_id(w),
    )
    .await
    .expect("emit(Ping 1) timed out")
    .unwrap();

    // (2) Wait until the first reaction ran, then a generous grace period so
    // every consumer's cursor passes Pong-1's position — the system is fully
    // quiescent before step 3.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while reacts.load(AtomicOrdering::SeqCst) < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "first reaction never ran — fixture broken, not the finding"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(
        reacts.load(AtomicOrdering::SeqCst),
        1,
        "exactly one reaction expected after quiescence"
    );

    // (3) Second emit into the SAME workflow, awaited via .settled().
    // Correct behavior: settled() returns only after the whole chain of
    // Ping(n=2) drained — the reactor must have processed it.
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        engine.emit(Ping { id: w, n: 2 }).workflow_id(w).settled(),
    )
    .await
    .expect("settled() wedged — different failure mode than the finding")
    .unwrap();

    let n = reacts.load(AtomicOrdering::SeqCst);
    assert_eq!(
        n, 2,
        "settled() returned before the just-emitted trigger was processed \
         (reactor ran {n} time(s); expected 2) — the stale workflow_hw entry \
         masked the emit-position floor"
    );

    engine.shutdown().await.unwrap();
}
