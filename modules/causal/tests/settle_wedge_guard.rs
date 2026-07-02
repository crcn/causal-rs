//! Repro for the `settle()` non-convergence (infinite hang) in the
//! partitioned runner — the multi-PerWorkflow-consumer / gated
//! dual-trigger shape from rootsignal-scout's synthesis cascade.
//!
//! Shape (all facts share SUBJECT="run", subject_id = run_id, and are
//! emitted as chain members with workflow_id = run_id):
//!
//!   - Coord aggregate folds Sim -> sim, Resp -> resp, Sev -> sev.
//!   - ReactSim (Trigger=Sim, PerWorkflow)  } shared gated body:
//!   - ReactResp (Trigger=Resp, PerWorkflow)} fire only when BOTH
//!     sim && resp folded and !sev; emits [Sev, Phase].
//!   - Several PerWorkflow consumers trigger on Phase; one emits the
//!     terminal Done after reading an aggregate via state_of().map_err(transient).
//!
//! Caller: emit(Sim).settled(), then emit(Resp).settled(). The second
//! settle is the one that drives the gated cascade.

use anyhow::Result;
use async_trait::async_trait;
use causal::aggregate::{Aggregate, Apply};
use causal::{Aggregator, Ctx, EngineBuilder, Event, Events, Ordering, Projector, Reactor, RetryPolicy};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Facts (all on SUBJECT="run", subject_id = run_id) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sim {
    run_id: Uuid,
}
impl Event for Sim {
    const NAME: &'static str = "sim";
    const SUBJECT: &'static str = "run";
    fn subject_id(&self) -> Uuid {
        self.run_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Resp {
    run_id: Uuid,
}
impl Event for Resp {
    const NAME: &'static str = "resp";
    const SUBJECT: &'static str = "run";
    fn subject_id(&self) -> Uuid {
        self.run_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sev {
    run_id: Uuid,
}
impl Event for Sev {
    const NAME: &'static str = "sev";
    const SUBJECT: &'static str = "run";
    fn subject_id(&self) -> Uuid {
        self.run_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum PhaseKind {
    Synthesis,
    Supervisor,
    Coalescing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Phase {
    run_id: Uuid,
    phase: PhaseKind,
}
impl Event for Phase {
    const NAME: &'static str = "phase";
    const SUBJECT: &'static str = "run";
    fn subject_id(&self) -> Uuid {
        self.run_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Done {
    run_id: Uuid,
}
impl Event for Done {
    const NAME: &'static str = "done";
    const SUBJECT: &'static str = "run";
    fn subject_id(&self) -> Uuid {
        self.run_id
    }
}

// ── Coord aggregate ──

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct Coord {
    sim: bool,
    resp: bool,
    sev: bool,
}
impl Aggregate for Coord {
    const NAME: &'static str = "Coord";
    const SUBJECT: &'static str = "run";
}
impl Apply<Sim> for Coord {
    fn apply(&mut self, _: &Sim) {
        self.sim = true;
    }
}
impl Apply<Resp> for Coord {
    fn apply(&mut self, _: &Resp) {
        self.resp = true;
    }
}
impl Apply<Sev> for Coord {
    fn apply(&mut self, _: &Sev) {
        self.sev = true;
    }
}

fn coord_aggregators() -> Vec<Aggregator> {
    vec![
        Aggregator::for_type::<Coord, Sim>(),
        Aggregator::for_type::<Coord, Resp>(),
        Aggregator::for_type::<Coord, Sev>(),
    ]
}

// ── Shared gated body for the two synthesis reactors ──

async fn infer(ctx: Ctx<'_>) -> Result<Events> {
    let coord = ctx
        .state_of::<Coord>(ctx.workflow_id)
        .await
        .map_err(causal::transient)?
        .curr;
    if !(coord.sim && coord.resp && !coord.sev) {
        return Ok(Events::new());
    }
    let mut out = Events::new();
    out.push(Sev { run_id: ctx.workflow_id });
    out.push(Phase { run_id: ctx.workflow_id, phase: PhaseKind::Synthesis });
    Ok(out)
}

struct ReactSim;
#[async_trait]
impl Reactor for ReactSim {
    type Trigger = Sim;
    const NAME: &'static str = "react.sim";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, _t: &Sim, ctx: Ctx<'_>) -> Result<Events> {
        infer(ctx).await
    }
}

struct ReactResp;
#[async_trait]
impl Reactor for ReactResp {
    type Trigger = Resp;
    const NAME: &'static str = "react.resp";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, _t: &Resp, ctx: Ctx<'_>) -> Result<Events> {
        infer(ctx).await
    }
}

// ── Phase consumers (multi-PerWorkflow-consumer) ──
//
// Mirrors rootsignal: RunCompletion fires on EVERY PhaseCompleted;
// RunSupervisor/CoalesceValve/SalienceCut each fire only for one phase
// kind and self-feed the next phase. So the chain is
//   Phase{Synthesis} -> Phase{Supervisor} -> Phase{Coalescing} -> (terminal)
// and RunCompletion emits a Done for each phase.

/// RunCompletion analog: reads an aggregate via state_of().map_err(transient),
/// emits Done for EVERY phase (fires on Synthesis, Supervisor, Coalescing).
struct ReactComplete;
#[async_trait]
impl Reactor for ReactComplete {
    type Trigger = Phase;
    const NAME: &'static str = "react.complete";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, _t: &Phase, ctx: Ctx<'_>) -> Result<Events> {
        let _coord = ctx
            .state_of::<Coord>(ctx.workflow_id)
            .await
            .map_err(causal::transient)?
            .curr;
        let mut out = Events::new();
        out.push(Done { run_id: ctx.workflow_id });
        Ok(out)
    }
}

/// RunSupervisor analog: Synthesis -> emit Phase{Supervisor}.
struct ReactSupervisor;
#[async_trait]
impl Reactor for ReactSupervisor {
    type Trigger = Phase;
    const NAME: &'static str = "react.supervisor";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, t: &Phase, ctx: Ctx<'_>) -> Result<Events> {
        if t.phase != PhaseKind::Synthesis {
            return Ok(Events::new());
        }
        let mut out = Events::new();
        out.push(Phase { run_id: ctx.workflow_id, phase: PhaseKind::Supervisor });
        Ok(out)
    }
}

/// CoalesceValve analog: Supervisor -> emit Phase{Coalescing}.
struct ReactCoalesce;
#[async_trait]
impl Reactor for ReactCoalesce {
    type Trigger = Phase;
    const NAME: &'static str = "react.coalesce";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, t: &Phase, ctx: Ctx<'_>) -> Result<Events> {
        if t.phase != PhaseKind::Supervisor {
            return Ok(Events::new());
        }
        let mut out = Events::new();
        out.push(Phase { run_id: ctx.workflow_id, phase: PhaseKind::Coalescing });
        Ok(out)
    }
}

/// SalienceCut analog: Coalescing -> terminal (early-return otherwise).
struct ReactSalience;
#[async_trait]
impl Reactor for ReactSalience {
    type Trigger = Phase;
    const NAME: &'static str = "react.salience";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, _t: &Phase, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(Events::new())
    }
}

/// Happy-path regression: the realistic multi-PerWorkflow-consumer gated
/// cascade (the shape that hangs downstream) still CONVERGES. This guards
/// against the wedge guard false-tripping on a healthy run — every
/// consumer here makes progress, so none is ever flagged.
#[tokio::test] // current_thread runtime — matches the downstream `#[tokio::test]`
async fn both_triggers_settle_converges() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("causal::settle=trace,causal=warn")
        .with_test_writer()
        .try_init();

    let engine = EngineBuilder::memory()
        .with_aggregators(coord_aggregators())
        .with_reactor(ReactSim)
        .with_reactor(ReactResp)
        .with_reactor(ReactComplete)
        .with_reactor(ReactSupervisor)
        .with_reactor(ReactCoalesce)
        .with_reactor(ReactSalience)
        .build()
        .await
        .unwrap();

    let run = Uuid::new_v4();

    // First trigger: gate not satisfied (resp absent) -> no emission.
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        engine.emit(Sim { run_id: run }).workflow_id(run).settled(),
    )
    .await
    .expect("settle(Sim) timed out")
    .unwrap();

    // Second trigger: gate satisfied -> emits [Sev, Phase] -> Phase
    // consumers fire -> ReactComplete emits Done. This is the settle
    // the brief reports as hanging.
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        engine.emit(Resp { run_id: run }).workflow_id(run).settled(),
    )
    .await
    .expect("settle(Resp) timed out — REPRODUCED THE HANG")
    .unwrap();

    let coord = engine
        .state_of::<Coord>(run)
        .await
        .unwrap()
        .expect("coord must exist");
    assert!(coord.sim && coord.resp && coord.sev, "cascade must complete");

    engine.shutdown().await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// Mechanism repro: a consumer that deterministically fails every step
// (panic OR Err) is retried forever by `supervise_one` with no parking
// ceiling, so its cursor never advances past the failing event — and
// `settle` waits for it to drain to high-water FOREVER. This is the
// silent, low-CPU, timing-sensitive liveness stall observed downstream.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kick {
    id: Uuid,
}
impl Event for Kick {
    const NAME: &'static str = "kick";
    const SUBJECT: &'static str = "k";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Boom {
    id: Uuid,
}
impl Event for Boom {
    const NAME: &'static str = "boom";
    const SUBJECT: &'static str = "k";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

/// Reactor: Kick -> Boom. Boom bumps the workflow high-water.
struct Kicker;
#[async_trait]
impl Reactor for Kicker {
    type Trigger = Kick;
    const NAME: &'static str = "kicker";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, t: &Kick, _ctx: Ctx<'_>) -> Result<Events> {
        let mut out = Events::new();
        out.push(Boom { id: t.id });
        Ok(out)
    }
}

/// Projector on Boom that ALWAYS errors with a TRANSIENT-classified
/// failure — models an infra/pg-backed projector failing in a test with no
/// backend. A transient error retries up to the liveness ceiling (hours),
/// so within the test window its cursor never passes Boom and it stays
/// wedged. (A bare/unclassified error would instead park after
/// `max_attempts` under the H1 poison-park policy — self-healing, which is
/// NOT what "infra failure with no backend" models.)
struct FailingBoomProjector;
#[async_trait]
impl Projector for FailingBoomProjector {
    type Event = Boom;
    const NAME: &'static str = "failing.boom.projector";
    async fn project(&self, _fact: &Boom, _ctx: Ctx<'_>) -> Result<()> {
        Err(causal::transient(anyhow::anyhow!(
            "simulated infra projector failure (no backend)"
        )))
    }
}

#[tokio::test] // current_thread — matches downstream
async fn failing_consumer_surfaces_instead_of_wedging_settle() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("causal::settle=trace,causal=warn")
        .with_test_writer()
        .try_init();

    let engine = EngineBuilder::memory()
        .with_reactor(Kicker)
        .with_projector(FailingBoomProjector)
        .build()
        .await
        .unwrap();

    let w = Uuid::new_v4();

    // Kicker emits Boom (hw advances to Boom's position). The projector
    // on Boom fails every step, so it can never drain to Boom. Before the
    // fix, settle polled `drained` forever (silent hang). Now settle must
    // RETURN — surfacing the wedged consumer as an error rather than
    // hanging. The 30s budget is generous vs. the ~4s wedge-surface time;
    // it only catches a regression to the old infinite-hang behavior.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        engine.emit(Kick { id: w }).workflow_id(w).settled(),
    )
    .await
    .expect("settle HUNG — regression: a wedged consumer is holding settle hostage forever");

    let err = result.expect_err("settle must surface the wedged consumer as an error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("wedged") && msg.contains("failing.boom.projector"),
        "settle error must name the wedged consumer; got: {msg}"
    );

    engine.shutdown().await.unwrap();
}

/// Reactor that fails its trigger with a `transient`-classified error
/// forever. Transient failures back off under a 6h ceiling (never park),
/// so the trigger never acks and `wf_pending` never clears — the second
/// wedge locus. A tiny fixed backoff keeps the test fast; production uses
/// whatever the reactor declares.
struct FlakyOnKick;
#[async_trait]
impl Reactor for FlakyOnKick {
    type Trigger = Kick;
    const NAME: &'static str = "flaky.on.kick";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    fn retry_policy(&self) -> Option<RetryPolicy> {
        Some(RetryPolicy::fixed(100_000, 1)) // 1ms backoff so the wedge surfaces in test time
    }
    async fn react(&self, _t: &Kick, _ctx: Ctx<'_>) -> Result<Events> {
        Err(causal::transient(anyhow::anyhow!("dependency permanently down")))
    }
}

#[tokio::test] // current_thread — matches downstream
async fn reactor_stuck_on_transient_surfaces_instead_of_wedging_settle() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("causal::settle=trace,causal=warn")
        .with_test_writer()
        .try_init();

    let engine = EngineBuilder::memory()
        .with_reactor(FlakyOnKick)
        .build()
        .await
        .unwrap();

    let w = Uuid::new_v4();

    // The reactor retries the Kick trigger transiently forever, so it can
    // never drain. Before the fix, settle would wait out the 6h transient
    // ceiling (i.e. hang for any practical purpose). Now it surfaces.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        engine.emit(Kick { id: w }).workflow_id(w).settled(),
    )
    .await
    .expect("settle HUNG — regression: a reactor stuck on transient is wedging settle");

    let err = result.expect_err("settle must surface the transient-wedged reactor");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("wedged") && msg.contains("flaky.on.kick"),
        "settle error must name the transient-wedged reactor; got: {msg}"
    );

    engine.shutdown().await.unwrap();
}
