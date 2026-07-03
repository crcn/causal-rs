//! AUDIT v20 — verifier test (temporary; deleted after the audit).
//!
//! Claim under test: a deterministic fold error that classify_structural
//! does NOT see as Poison (e.g. the stream-alignment bail in
//! `AggregatorRegistry::apply_event` — a raw `anyhow::bail!`, no
//! serde_json::Error in the chain, no ClassifiedError) parks as
//! `unclassified` in life 1 (cursor advances past it), and then in life 2
//! `ensure_hydrated` re-hits the identical error at a position <= cursor,
//! classification != Poison, so the hydration skip branch never fires and
//! the consumer is wedged at every boot.
//!
//! CORRECT behavior (the stated invariant of the hydration skip branch:
//! "any event at position <= cursor was, in a prior life, either folded Ok
//! or parked") would skip the previously-parked event and keep the
//! pipeline live after restart. The assertions below encode CORRECT
//! behavior; the defect makes them fail.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::types::LogCursor;
use causal::{
    CheckpointStore, Ctx, Engine, EngineBuilder, Event, EventLogBackend, MemoryStore, Projector,
    ReactorCheckpoint,
};

// Fact that streams under its own kind ("v20_fact-{id}").
#[derive(Clone, Serialize, Deserialize)]
struct V20Fact {
    id: Uuid,
}
impl Event for V20Fact {
    const NAME: &'static str = "v20_fact";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

// Benign fact used as a liveness probe for the pipeline.
#[derive(Clone, Serialize, Deserialize)]
struct V20Ok {
    id: Uuid,
}
impl Event for V20Ok {
    const NAME: &'static str = "v20_ok";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

// Restorable aggregate (SUBJECT non-empty) whose declared subject does NOT
// match V20Fact's stream ("v20_fact"). Every fold of V20Fact trips the
// deterministic stream-alignment bail in `apply_event` — a raw anyhow
// error that classify_structural returns None for.
#[derive(Default, Clone, Serialize, Deserialize)]
struct V20State {
    n: u64,
}
impl Aggregate for V20State {
    const NAME: &'static str = "V20State";
    const SUBJECT: &'static str = "v20_other_stream";
}
impl Apply<V20Fact> for V20State {
    fn apply(&mut self, _e: &V20Fact) {
        self.n += 1;
    }
}

struct OkProjector;
#[async_trait]
impl Projector for OkProjector {
    type Event = V20Ok;
    const NAME: &'static str = "v20.ok.projector";
    async fn project(&self, _f: &V20Ok, _ctx: Ctx<'_>) -> Result<()> {
        Ok(())
    }
}

async fn build(mem: &Arc<MemoryStore>) -> Engine {
    EngineBuilder::new(
        mem.clone() as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators([Aggregator::for_type::<V20State, V20Fact>()])
    .with_projector(OkProjector)
    .with_max_attempts(1) // park on the first deterministic failure
    .build()
    .await
    .expect("engine build")
}

#[tokio::test]
async fn parked_unclassified_fold_error_must_not_wedge_hydration_after_restart() {
    let mem = Arc::new(MemoryStore::new());

    // ── Life 1: park + advance keeps the pipeline live ────────────────
    {
        let engine = build(&mem).await;

        // The projector folds EVERY event (aggregators attached) before
        // prefix-matching. V20Fact hits the alignment bail; with
        // max_attempts = 1 it parks immediately and the cursor advances,
        // so settled() must resolve.
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            engine.emit(V20Fact { id: Uuid::new_v4() }).settled(),
        )
        .await
        .expect("life 1: settled() HUNG — park+advance path broken (test premise)")
        .expect("life 1: settled() errored — expected park+advance to keep the pipeline live");

        // Benign probe in the same life still settles.
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            engine.emit(V20Ok { id: Uuid::new_v4() }).settled(),
        )
        .await
        .expect("life 1: benign settled() HUNG")
        .expect("life 1: benign settled() errored");

        engine.shutdown().await.expect("life 1 shutdown");
    }

    // The park must be on record; capture its class label for diagnostics.
    let all = EventLogBackend::read_all(mem.as_ref(), LogCursor::ZERO, 1024)
        .await
        .expect("read_all");
    let parked: Vec<_> = all
        .iter()
        .filter(|e| e.event_type == "causal:projection_failed")
        .collect();
    assert!(
        !parked.is_empty(),
        "life 1 must have parked the deterministically-failing fold"
    );
    let class = parked[0]
        .payload
        .get("class")
        .and_then(|c| c.as_str())
        .unwrap_or("?")
        .to_string();
    eprintln!("AUDIT v20: life-1 park recorded with class = {class:?}");

    // ── Life 2: same store, fresh process ──────────────────────────────
    let engine2 = build(&mem).await;

    // CORRECT behavior: hydration skips the previously-parked event
    // (cursor already passed it in life 1) and the pipeline is live.
    // DEFECT behavior: ensure_hydrated re-hits the alignment bail,
    // classification != Poison, Err propagates, the OnceCell never
    // initializes, every step() fails — settle surfaces the consumer as
    // wedged (or hangs).
    let settled = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        engine2.emit(V20Ok { id: Uuid::new_v4() }).settled(),
    )
    .await
    .expect(
        "life 2: settled() HUNG — consumer wedged at boot: hydration cannot pass the \
         event parked in life 1",
    );
    settled.unwrap_or_else(|e| {
        panic!(
            "life 2: settled() errored — consumer wedged at boot, re-hitting the parked \
             (class={class}) deterministic fold error during hydration: {e:#}"
        )
    });

    engine2.shutdown().await.expect("life 2 shutdown");
}
