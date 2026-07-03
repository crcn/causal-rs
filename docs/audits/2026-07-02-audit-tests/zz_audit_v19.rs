//! Audit verifier test for finding #19: a projector whose DEPENDS_ON
//! names a consumer that will never exist (typo / rename / dep in a
//! different process) wedges `settle()` forever, and every settle guard
//! (wedge failure counter, worker_stall, opt-in liveness ceiling) is
//! blind to it because WaitOnDep is counted as progress.
//!
//! CONTRACT under test (established by settle_wedge_guard.rs): a consumer
//! that can NEVER drain must be surfaced by settle as an error — never a
//! silent infinite hang. So correct behavior = the second settled() below
//! returns (Ok or Err) within the window; the defect = it times out.

use anyhow::Result;
use async_trait::async_trait;
use causal::{Ctx, EngineBuilder, Event, Projector};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Tick {
    id: Uuid,
}
impl Event for Tick {
    const NAME: &'static str = "tick";
    const SUBJECT: &'static str = "audit";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

/// Body always succeeds; the only pathology is the phantom dependency.
struct DepFenced;
#[async_trait]
impl Projector for DepFenced {
    type Event = Tick;
    const NAME: &'static str = "dep.fenced.projector";
    const DEPENDS_ON: &'static [&'static str] = &["consumer.that.never.exists"];
    async fn project(&self, _f: &Tick, _ctx: Ctx<'_>) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn missing_dep_wedges_settle_silently_despite_all_guards() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("causal::settle=trace,causal=warn")
        .with_test_writer()
        .try_init();

    let engine = EngineBuilder::memory()
        .with_projector(DepFenced)
        .build()
        .await
        .unwrap()
        // Opt into the D3 liveness ceiling to prove even it cannot see
        // this wedge: WaitOnDep stamps note_progress every poll cycle,
        // so idle_for() never exceeds the ceiling.
        .with_settle_liveness_ceiling(Some(std::time::Duration::from_secs(2)));

    // First emit: the dep fence passes while the projector's own cursor
    // is still ZERO (dep ZERO is not < cursor ZERO), so the first batch
    // folds tick-1 and the cursor advances to its position. This settle
    // must succeed — it also guarantees the cursor is durably past ZERO
    // before the second emit, making the wedge deterministic.
    let w1 = Uuid::new_v4();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        engine.emit(Tick { id: w1 }).workflow_id(w1).settled(),
    )
    .await
    .expect("first settle HUNG — unexpected; the dep fence passes at cursor ZERO")
    .expect("first settle errored");

    // Second emit: cursor > ZERO now, phantom dep's cursor is forever
    // ZERO (no checkpoint row), so every step returns WaitOnDep and the
    // cursor freezes below tick-2's position. If the guards work, settle
    // surfaces an error; if the defect is real, this times out.
    let w2 = Uuid::new_v4();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        engine.emit(Tick { id: w2 }).workflow_id(w2).settled(),
    )
    .await;

    match outcome {
        Ok(res) => {
            // Correct behavior: either drained (impossible here) or the
            // wedge was surfaced as an error. Both mean the guards work.
            eprintln!("settle returned within budget: {res:?}");
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                engine.shutdown(),
            )
            .await;
        }
        Err(_elapsed) => {
            panic!(
                "DEFECT CONFIRMED: settle hung >20s on a projector wedged by \
                 DEPENDS_ON naming a nonexistent consumer. WaitOnDep resets \
                 the wedge failure counter and the liveness heartbeat every \
                 poll, worker_stall is None for projectors — no guard fires."
            );
        }
    }
}
