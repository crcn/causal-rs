//! Fan-in race tests for filter and transition guard.
//!
//! These tests reproduce two related framework bugs:
//!
//! 1. **Filter race** — `filter_fn` is evaluated at Phase 2 inside the boxed
//!    reactor closure (`reactor/builders.rs:656-668`). When N events for the
//!    same reactor land in one Phase 1 batch, all N closures run in parallel
//!    via `join_all` and each evaluates the filter against the *same*
//!    post-batch aggregator state. State-based gates intended to fire once
//!    fire N times (or zero, depending on direction).
//!
//! 2. **Transition guard race** — `.transition::<A>(...)` calls
//!    `registry.get_transition::<A>(id)` inside the same Phase 2 closure
//!    (`reactor/builders.rs:780-806`). The aggregator stores `:prev` as a
//!    single DashMap slot per aggregate id (`aggregator.rs:263`), overwritten
//!    on every fold. By the time Phase 2 closures read, `:prev` is the *last*
//!    event's prev — not theirs. Existing `transition_*` tests in
//!    `stress_tests.rs` only pass because they cross `.settled()` between
//!    every emit, forcing one event per Phase 1 batch and masking the race.
//!
//! Each test here emits multiple events without intervening `.settled()`,
//! so they all sit in the log and get pulled into a single Phase 1 batch.
//! The expected post-fix behavior is the assertion; the today-broken behavior
//! is documented in a comment so the failure mode is unambiguous when these
//! go red.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use causal::{event, events, reactor, Aggregate, Apply, Context, Engine};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone)]
struct Deps;

// ── Test 1 events + aggregate ──────────────────────────────────────────

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Tick {
    seq: u32,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TickCounter {
    count: usize,
}

impl Aggregate for TickCounter {
    fn aggregate_type() -> &'static str {
        "TickCounter"
    }
}

impl Apply<Tick> for TickCounter {
    fn apply(&mut self, _event: Tick) {
        self.count += 1;
    }
}

// ── Test 2/3 events + aggregate ────────────────────────────────────────

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetA;

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetB;

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetC;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
enum Phase {
    #[default]
    Init,
    A,
    B,
    C,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PhaseState {
    phase: Phase,
}

impl Aggregate for PhaseState {
    fn aggregate_type() -> &'static str {
        "PhaseState"
    }
}

impl Apply<SetA> for PhaseState {
    fn apply(&mut self, _: SetA) {
        self.phase = Phase::A;
    }
}

impl Apply<SetB> for PhaseState {
    fn apply(&mut self, _: SetB) {
        self.phase = Phase::B;
    }
}

impl Apply<SetC> for PhaseState {
    fn apply(&mut self, _: SetC) {
        self.phase = Phase::C;
    }
}

// ═══════════════════════════════════════════════════════════════════════
// TEST 1: filter under fan-in
// ═══════════════════════════════════════════════════════════════════════

/// Emit 50 `Tick` events into a single Phase 1 batch. The filter passes
/// only on the *first* tick — `count == 1` after that tick's fold.
///
/// **Today (buggy):** filter is evaluated in Phase 2 against the post-batch
/// state where `count == 50`. All 50 closures see `count == 50`, the filter
/// returns false for every one, the reactor fires **0 times**.
///
/// **After the fix** (filter eval at intent-build time, against per-event
/// post-fold state): only tick #1's filter sees `count == 1`, so the reactor
/// fires **exactly once**.
#[tokio::test]
async fn filter_fires_exactly_once_when_state_set_by_first_event_in_batch() -> Result<()> {
    let fire_count = Arc::new(AtomicUsize::new(0));
    let fc = fire_count.clone();

    let engine = Engine::in_memory(Deps)
        .with_aggregator::<Tick, TickCounter, _>(|_| Uuid::nil())
        .with_reactor(
            reactor::on::<Tick>()
                .filter(|_event, ctx: &Context<Deps>| {
                    // Fire only on the first tick: count is set by Apply<Tick>
                    // on the same event, so per-event eval should see count==1
                    // for tick #1 and count>=2 for the rest.
                    ctx.aggregate::<TickCounter>().curr.count == 1
                })
                .then(move |_event, _ctx: Context<Deps>| {
                    let fc = fc.clone();
                    async move {
                        fc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    // Publish 50 ticks WITHOUT settling between them — they accumulate in
    // the log so the next settle() pulls them in one Phase 1 batch.
    for seq in 0..50 {
        engine.emit(Tick { seq }).await?;
    }
    engine.settle().await?;

    assert_eq!(
        fire_count.load(Ordering::SeqCst),
        1,
        "filter that's true on count==1 should fire exactly once across a 50-event batch \
         (today fires 0 times because all 50 closures see post-batch count=50)"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// TEST 2: transition guard under fan-in (single :prev slot)
// ═══════════════════════════════════════════════════════════════════════

/// Emit `[SetA, SetB, SetB, SetB, SetB]` into one Phase 1 batch. The first
/// `SetB` is a real `A → B` transition; the rest are `B → B` no-ops.
/// A transition guard `prev == A && next == B` should fire **exactly once**.
///
/// **Today (buggy):** the `:prev` DashMap slot is overwritten on every fold,
/// so by the time Phase 2 closures call `get_transition`, prev/next reads as
/// `(B, B)` for every closure. Guard returns false for all four `SetB`
/// intents → fires **0 times**.
///
/// **After the fix:** evaluate transition with the per-event `(pre, post)`
/// captured around each fold, in Phase 1. Only the first `SetB` sees
/// `(A, B)` → fires once.
#[tokio::test]
async fn transition_fires_once_for_real_transition_buried_in_batch_of_noops() -> Result<()> {
    let fire_count = Arc::new(AtomicUsize::new(0));
    let fc = fire_count.clone();

    let engine = Engine::in_memory(Deps)
        .with_aggregator::<SetA, PhaseState, _>(|_| Uuid::nil())
        .with_aggregator::<SetB, PhaseState, _>(|_| Uuid::nil())
        .with_reactor(
            reactor::on::<SetB>()
                .extract(|_| Some(Uuid::nil()))
                .transition::<PhaseState, _>(|prev, next| {
                    prev.phase == Phase::A && next.phase == Phase::B
                })
                .then(move |_id: Uuid, _ctx: Context<Deps>| {
                    let fc = fc.clone();
                    async move {
                        fc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    engine.emit(SetA).await?;
    engine.emit(SetB).await?;
    engine.emit(SetB).await?;
    engine.emit(SetB).await?;
    engine.emit(SetB).await?;
    engine.settle().await?;

    assert_eq!(
        fire_count.load(Ordering::SeqCst),
        1,
        "transition prev==A && next==B should fire exactly once across the batch \
         (today fires 0 times because :prev gets overwritten by each fold and \
          all closures read (B,B) at Phase 2)"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// TEST 3: in-batch a → b → c → b transition
// ═══════════════════════════════════════════════════════════════════════

/// Emit `[SetA, SetB, SetC, SetB]` into one Phase 1 batch. The aggregate
/// transitions A → B → C → B. A guard `prev == A && next == B` should fire
/// **exactly once** — at the first `SetB`, which is the only event that
/// satisfies the guard.
///
/// **Today (buggy):** at end of Phase 1 the `:prev` slot reads `C` and
/// current reads `B` (last fold's prev/next). Both `SetB` closures call
/// `get_transition` and see `(C, B)`. Guard returns false → fires
/// **0 times**.
///
/// **After the fix** (with `:prev` slot replaced by per-event stack-local
/// `(pre, post)` snapshots): the first `SetB` evaluates against `(A, B)`
/// and fires; the second `SetB` evaluates against `(C, B)` and skips.
/// Total: **1 fire**.
///
/// This test specifically proves the fix has to *kill* the `:prev` slot —
/// not just relocate the read into Phase 1. A relocation that still uses
/// the slot would still fail, because intermediate `SetC` overwrites it
/// before the second `SetB` is evaluated.
#[tokio::test]
async fn transition_fires_once_for_a_to_b_buried_in_a_b_c_b_batch() -> Result<()> {
    let fire_count = Arc::new(AtomicUsize::new(0));
    let fc = fire_count.clone();

    let engine = Engine::in_memory(Deps)
        .with_aggregator::<SetA, PhaseState, _>(|_| Uuid::nil())
        .with_aggregator::<SetB, PhaseState, _>(|_| Uuid::nil())
        .with_aggregator::<SetC, PhaseState, _>(|_| Uuid::nil())
        .with_reactor(
            reactor::on::<SetB>()
                .extract(|_| Some(Uuid::nil()))
                .transition::<PhaseState, _>(|prev, next| {
                    prev.phase == Phase::A && next.phase == Phase::B
                })
                .then(move |_id: Uuid, _ctx: Context<Deps>| {
                    let fc = fc.clone();
                    async move {
                        fc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    engine.emit(SetA).await?;
    engine.emit(SetB).await?;
    engine.emit(SetC).await?;
    engine.emit(SetB).await?;
    engine.settle().await?;

    assert_eq!(
        fire_count.load(Ordering::SeqCst),
        1,
        "guard prev==A && next==B should fire once on the first SetB in [A,B,C,B] batch \
         (today fires 0 times because :prev was overwritten to C by the time Phase 2 reads it)"
    );
    Ok(())
}
