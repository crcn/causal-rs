# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] - 2026-05-04

### Fixed

**Aggregator double-apply across projection-failure retries (regression in 0.2.0).**

In 0.2.0, when a projection returned `Err`, the engine would `break` and retry
the event on the next settle iteration. But the per-event flow applied the
event to aggregator state *before* `process_event_inner` ran:

```
hydrate_for_event → apply_to_aggregators (mutates state) → process_event_inner
```

On retry, `hydrate_for_event` saw state already existed (no rehydrate) and
`apply_to_aggregators` ran *again* on the post-mutation state. After
`max_event_retry_attempts` retries, the aggregate had the event applied
N+1 times — silent state corruption for accumulator-style aggregates
(counters, totals). Idempotent UPSERT-style aggregates were unaffected.

The fix:

1. **Capture rollback before apply.** A new `AggregatorRegistry::capture_for_rollback`
   snapshots pre-mutation state; on `Err` from `process_event_inner`, the
   engine restores it via `restore_state`. Retries see the correct
   pre-mutation state and apply exactly once per attempt.
2. **Apply on park.** When the retry budget exhausts and the event parks,
   the engine applies the event one final time (without rollback) so live
   aggregator state matches what cold-start replay would reconstruct from
   the event log. The log is the source of truth — parked events are still
   facts that happened.
3. **Defer snapshots to success path.** `maybe_auto_snapshot` previously
   ran inside the per-event mutation block, which could write a snapshot
   reflecting an event that subsequently failed. Now snapshots are taken
   only in the success branch and the park branch — never during a
   transient retry.

**Park reason now carries the underlying projection error.**

Pre-fix: `process_event_inner` returned `Err`, the engine did `Err(_e) => break;`,
the error was discarded. After retries, the event parked with a generic
`"Event failed after 3 retry attempts"` reason. Operators querying the
DLQ surface had no signal about which projection failed or what error
it returned.

Now: the engine captures the last error string per event position and
includes it in the park reason. Park reasons read like
`"Event failed after 3 retry attempts: 1 projection(s) failed for event
<uuid>: schedule_state: connection refused"`.

The error is also logged via `tracing::warn!` on each Err so it's visible
in traces during retry attempts, not just at park time.

### Added

- `MemoryStore::parked_events() -> Vec<(Uuid, String)>` — returns the
  list of `(event_id, park_reason)` pairs recorded by `IntentCommit::park`.
  Useful for tests asserting park behavior; also surfaces the same data
  that downstream backends would persist to a DLQ table.

### Internal

- `AggregatorRegistry::capture_for_rollback(event_type, payload) -> AggregatorRollback`
  and `AggregatorRegistry::restore_state(rollback)` — `pub(crate)` API
  used by the engine to roll back aggregator state on transient
  `process_event_inner` failures. Not part of the public API; the surface
  to expose this for downstream backends will be designed if needed.

### Tests

- `aggregator_not_double_applied_when_projection_fails` — regression test
  that fails on 0.2.0 (counter=3) and passes on 0.2.1 (counter=1).
- `park_reason_carries_projection_error_message` — proves the park reason
  contains the failing projection id and underlying error.

### Migration from 0.2.0

No code changes for callers. **Strongly recommended** to upgrade if you
have aggregators that accumulate state (counters, sums, lists, etc.) —
0.2.0 silently corrupts them on projection failure. UPSERT-style
aggregates (status fields, last-write-wins) are safe in both versions.

## [0.2.0] - 2026-05-04

### Breaking

**Projection failure no longer silently advances the dispatch cursor.**

Prior to this release, when a projection returned `Err`, the engine recorded
the failure into `IntentCommit::projection_failures`, advanced the dispatch
cursor anyway, and `MemoryStore::enqueue` printed and discarded the failure.
The event was treated as "processed" even though its derived state was never
written. For event-sourced consumers using projections to maintain queryable
state (e.g. updating `next_run_at` in response to `ScheduleTriggered`), this
caused silent state divergence: the event was durable but the derived state
wasn't, and no retry happened.

Now: any projection returning `Err` causes `JobExecutor::process_event_inner`
to return `Err`. The dispatch cursor does not advance. The engine retries
the event via the existing event-retry budget
(`EventWorkerConfig::max_event_retry_attempts`, default 3); after the budget
is exhausted, the event parks with a descriptive reason. Reactors never fire
for parked events.

#### Migration

1. **Audit projections for idempotency.** Projections were always required
   to be idempotent (the existing `queue.enqueue`-fails-after-projection-
   succeeds path also required it), but the contract was rarely exercised.
   The new fail-closed semantics exercise it on every projection error.
   Any projection that does blind `INSERT`, increments a counter, or sends
   a notification must be fixed before upgrading. Notifications belong in
   reactors with `ctx.run()`, not projections.

2. **Watch the parked-event count after deploy.** Projection failures that
   were previously silent now surface as parked events. A previously-quiet
   broken system may produce a burst of parks on first deploy.

3. **No code changes required for compliant consumers.** The success path
   is unchanged. Only the failure path differs.

If you want a side effect that should NOT block dispatch on failure, use a
reactor — reactors have their own retry/DLQ and don't block the cursor. An
opt-in async projection mode is being designed; see
`docs/plans/2026-05-04-feat-async-projections-plan.md`.

### Removed

- `causal::types::ProjectionFailure` (was a public re-export). Projections
  now signal failure via `Err` from their reactor function; the engine
  routes failures through the normal event-retry path.
- `IntentCommit::projection_failures` field. The type now has a single
  responsibility: atomically enqueue intents and advance the dispatch
  cursor.

### Changed

- `Projection` rustdoc now states the idempotency contract and failure
  semantics explicitly.
- `JobExecutor::process_event_inner` returns `Err` when any projection
  fails, with a summary error message listing the failed projection IDs.

### Known limitations (filed as follow-ups)

- The event-retry counter at `engine.rs:371` is in-memory and resets on
  process restart. Under CrashLoopBackoff, a persistent projection bug
  could retry forever and never park.
- No backoff between event retries — transient failures can burn the retry
  budget in milliseconds.
- No per-event-per-projection success ledger — when one of several
  projections fails, all projections re-run on retry (idempotency is
  load-bearing for correctness, not just cleanliness).

These are tracked separately and will be addressed in subsequent releases.

## [0.1.7] - 2026-05-02

### Fixed

**Fan-in race in `.filter(...)` and `.transition::<A>(...)` gates.**

When multiple events for the same reactor landed in a single Phase 1 batch
(e.g. via several `engine.emit(...)` calls before a `settle()`), Phase 2
closures ran concurrently against shared aggregator state. Both
state-based filters and transition guards evaluated against the *post-batch*
state instead of their per-event state — so a "fire once" gate could fire
N times, or zero. The bug was structural, not a typo: filters lived inside
the boxed reactor closure, transition guards read prev/next from a single
DashMap `:prev` slot that every fold overwrites.

Filters and transition guards are now evaluated in Phase 1, at intent-build
time, against per-event post-fold state captured as stack-locals around the
fold. Reactors whose gate rejects no longer enqueue an intent at all — no
Phase 2 closure runs for them.

The user-facing builder API (`.filter(...)`, `.transition::<A>(guard)`)
is unchanged. Code that relied on the buggy behavior — e.g. trampoline
flags whose gate "happened to" fire because of the race — will now behave
as the gate's logic actually reads.

### Added

- `causal::aggregator::TransitionSnapshots` — per-event `(prev, next)`
  aggregate snapshots produced by `AggregatorRegistry::apply_event`.
  `get_pair::<A>(id) -> Option<(&A, &A)>` for transition guards that need
  the pair without racing on the registry's `:prev` slot.

### Changed

- `AggregatorRegistry::apply_event` now returns `TransitionSnapshots`
  instead of `()`. Callers using only the engine builder API are
  unaffected.
- `JobExecutor::process_event_inner` takes a new
  `&TransitionSnapshots` parameter. The `process_event` wrapper is
  unchanged.
- The cold-start path inside `apply_event` no longer early-returns after
  the first defaulted aggregator — multi-aggregator events now fold every
  match instead of skipping later ones (separate latent bug, fixed in
  passing).

### Internal

- `Reactor<D>` gained two `pub(crate)` fields, `intent_filter` and
  `intent_transition`, plus matching `passes_*` accessors. Builders now
  populate these instead of baking the gate into the Phase 2 reactor
  closure.
- The legacy `:prev` DashMap slot and `get_transition*` /
  `get_singleton*` registry readers are still written and accessible for
  backward compatibility, but the framework no longer relies on them for
  guard correctness. They remain racy under fan-in and should be
  considered soft-deprecated for new code.

## [0.5.0] - 2026-02-02

### 🚨 BREAKING CHANGES - Complete Architecture Overhaul

Migrated from Shay's Redux-style stateful engine to a **stateless, per-execution architecture** optimized for distributed edge functions.

### Changed - Breaking

**Engine is Now Stateless**
- Engine no longer stores state internally
- State is passed per-activation via `activate(initial_state)`
- Removed `Engine::state()` method
- State is scoped to Handle lifetime only

**API Pattern Changes**
- `Engine::new(state)` → `Engine::new()` (no state parameter)
- `Engine::with_deps(state, deps)` → `Engine::with_deps(deps)`
- `activate()` → `activate(initial_state)` (now requires state)
- Added `Handle::run()` for executing actions with results

**Closure-Based API** (from Shay)
- Replaced trait implementations with builder pattern
- `effect::on::<Event>().run(|event, ctx| async { ... })`
- `reducer::on::<Event>().run(|state, event| { ... })`
- No more `#[async_trait]` or trait implementations needed

**Effect Context Changes**
- Removed: `state()`, `correlation_id()`, `outbox_correlation_id()`
- Added: `prev_state()`, `next_state()`, `curr_state()`, `within()`
- Effects see both pre and post-reducer state

### Added

- **`Handle::run()` method**: Execute actions and return results
- **Pipedream integration**: Stream composition via pipedream-rs
- **State transition filters**: `effect::on().transition()`
- **Event filters**: `effect::on().filter()`
- **Lifecycle hooks**: `effect::on().started()`

### Removed

- `EngineBuilder` - use `Engine::new()` directly
- `RunContext` - use `Handle::run()` instead
- `engine.run(closure, state)` - use `activate(state).run()`
- State storage in Engine
- Trait-based Effect/Reducer API (use closure builders)

### Migration Guide

**Before (v0.4.0 - Trait-based):**
```rust
// Define effect struct
struct ShipEffect;

#[async_trait]
impl Effect<OrderEvent, Deps, State> for ShipEffect {
    type Event = OrderEvent;

    async fn handle(&mut self, event: OrderEvent, ctx: EffectContext) -> Result<()> {
        match event {
            OrderEvent::Placed { order_id } => {
                ctx.deps().ship(order_id).await?;
                ctx.emit(OrderEvent::Shipped { order_id });
                Ok(())
            }
            _ => Ok(())
        }
    }
}

// Create engine with state
let engine = EngineBuilder::new(deps)
    .with_effect::<OrderEvent, _>(ShipEffect)
    .build();

// Execute
engine.run(|ctx| {
    ctx.emit(OrderEvent::Placed { order_id });
    Ok(())
}, initial_state).await?;
```

**After (v0.5.0 - Closure-based, stateless):**
```rust
// Define engine once (stateless, reusable)
let engine = Engine::new()
    .with_effect(effect::on::<OrderEvent>().run(|event, ctx| async move {
        if let OrderEvent::Placed { order_id } = event.as_ref() {
            ctx.deps().ship(*order_id).await?;
            ctx.emit(OrderEvent::Shipped { order_id: *order_id });
        }
        Ok(())
    }));

// Execute per-request
let handle = engine.activate(State::default());
let result = handle.run(|ctx| {
    ctx.emit(OrderEvent::Placed { order_id });
    Ok(Response { status: "ok" })
})?;
handle.settled().await?;
```

See [API_MIGRATION.md](./API_MIGRATION.md) for complete migration guide.

## [0.4.0] - 2025-01-31

### Changed

- Updated `Effect::handle()` to return `Result<Option<Event>>` instead of `Result<Event>`
- Effects can now skip unhandled events by returning `Ok(None)`
- Removed need for verbose `unreachable!()` blocks in effect reactors

### Migration Guide

Before (v0.3.x):
```rust
async fn handle(&mut self, event: E, ctx: EffectContext<D, S>) -> Result<E> {
    match event {
        Event::Started => {
            Ok(Event::Completed)
        }
        _ => unreachable!("This effect only handles Started events")
    }
}
```

After (v0.4.0):
```rust
async fn handle(&mut self, event: E, ctx: EffectContext<D, S>) -> Result<Option<E>> {
    match event {
        Event::Started => {
            Ok(Some(Event::Completed))
        }
        _ => Ok(None) // Clean skip
    }
}
```

[0.5.0]: https://github.com/crcn/causal-rs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/crcn/causal-rs/compare/v0.3.0...v0.4.0
