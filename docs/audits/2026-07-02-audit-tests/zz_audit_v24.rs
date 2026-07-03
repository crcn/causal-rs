//! Audit verifier #24 — does the documented empty-batch flush idiom
//! (`engine.emit(Vec::new())` then `settle(result)`) actually wait for
//! pre-existing pending work of OTHER workflows to drain?
//!
//! EmitResult doc (engine.rs): "For an empty-batch emit, `position` is
//! the log's current latest position so a downstream `settle(result)`
//! waits for any pre-existing pending work to drain."
//!
//! Claim under test: since the workflow-scoped drained() probe, the
//! fresh random workflow_id on the empty emit matches no pending work,
//! so settle returns the moment the reactor's ingest cursor reaches the
//! tip — while a reaction for another workflow is still mid-flight.

use anyhow::Result;
use async_trait::async_trait;
use causal::{Ctx, EngineBuilder, Event, Events, Ordering, Reactor};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kick {
    id: Uuid,
}
impl Event for Kick {
    const NAME: &'static str = "audit24.kick";
    const SUBJECT: &'static str = "audit24";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KickDone {
    id: Uuid,
}
impl Event for KickDone {
    const NAME: &'static str = "audit24.kickdone";
    const SUBJECT: &'static str = "audit24";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

/// Reactor whose react() is slow: signals `started`, sleeps, then sets
/// `finished` and emits an output. If the flush barrier honors its doc,
/// settle(empty_result) must not return until `finished` is true.
struct SlowReactor {
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

#[async_trait]
impl Reactor for SlowReactor {
    type Trigger = Kick;
    const NAME: &'static str = "audit24.slow";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, t: &Kick, _ctx: Ctx<'_>) -> Result<Events> {
        self.started.store(true, AtomicOrdering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        self.finished.store(true, AtomicOrdering::SeqCst);
        let mut out = Events::new();
        out.push(KickDone { id: t.id });
        Ok(out)
    }
}

#[tokio::test]
async fn empty_emit_settle_waits_for_in_flight_reaction_of_other_workflow() {
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));

    let engine = EngineBuilder::memory()
        .with_reactor(SlowReactor {
            started: started.clone(),
            finished: finished.clone(),
        })
        .build()
        .await
        .unwrap();

    // (1) Fire-and-forget: real work in its own (auto) workflow X.
    let _r1 = engine.emit(Kick { id: Uuid::new_v4() }).await.unwrap();

    // (2) Wait until the reactor has ingested the trigger and its slow
    //     react() is mid-flight (ingest_pos is at the tip by now).
    let wait_started = async {
        while !started.load(AtomicOrdering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait_started)
        .await
        .expect("reactor never started its reaction — harness wedge");

    assert!(
        !finished.load(AtomicOrdering::SeqCst),
        "harness: reaction finished before the barrier was even placed"
    );

    // (3) The documented flush idiom: empty emit, then settle on it.
    let flush = engine.emit(Vec::<Kick>::new()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), engine.settle(flush))
        .await
        .expect("settle(flush) wedged — did not terminate")
        .expect("settle(flush) errored");

    // (4) Doc promise: settle "waits for any pre-existing pending work
    //     to drain". The reaction for workflow X was pre-existing
    //     pending work; it must have completed before settle returned.
    assert!(
        finished.load(AtomicOrdering::SeqCst),
        "DEFECT: settle(empty-emit result) returned while a pre-existing \
         reaction of another workflow was still mid-flight — the documented \
         flush barrier does not hold under the workflow-scoped drained() probe"
    );

    engine.shutdown().await.unwrap();
}
