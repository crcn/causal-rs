# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.5] - 2026-05-06

Operational hardening + documentation. Zero public API changes;
nothing for downstream callers to migrate.

### Fixed

- **Supervisor catches consumer panics.** Before: a panic inside
  `Materializer::materialize`, `Reactor::react`, or
  `AnyMaterializer::materialize` (e.g. `ctx.aggregate` called without
  aggregators registered) would unwind the spawned tokio supervisor
  task and `Engine` would silently lose the consumer — cursor never
  advanced, no log event surfaced, only discoverable by noticing
  consumer lag in production. Now: `supervise_one` wraps each
  `step()` call in `AssertUnwindSafe + catch_unwind`, emits an
  ERROR-level `tracing` event with the panic message, and applies
  the same backoff retry as the existing `Err(_)` recovery path.

### Documentation

- **`Fact::occurred_at` determinism contract.** Override values MUST
  be reproducible from the fact's serde representation. Reading from
  `#[serde(skip)]` fields, computed-on-construction values, or
  non-deterministic sources (`Utc::now()`) silently breaks replay
  determinism. Doc updated with the formal invariant and concrete
  guidance.

- **`EventLogBackend::append` `created_at` semantics.** The
  client-supplied `NewEvent::created_at` is a hint, not authoritative.
  Backends MAY override with a server-assigned timestamp on write —
  KurrentDB does this unconditionally; `MemoryStore` preserves the
  client value. Replay determinism follows from the persisted value.
  Doc spec'd to prevent surprises when a Kurrent backend lands.

- **`Reactor` self-feedback footgun.** A reactor whose output Events
  include a fact matching its own `Trigger::type_prefix()` reacts to
  its own output ad infinitum. Framework does not detect this;
  consumer discipline required. Doc lists two mitigation patterns
  (disjoint prefixes; metadata flag).

### Tests

`supervisor_recovers_from_consumer_panic` — TDD'd: materializer
panics on first call, succeeds on retry. Failed RED before the fix
("supervisor died on panic; consumer never recovered"), passes GREEN
after.

`with_aggregators_chainable_accumulation` — regression guard for the
two-call pattern rootsignal uses
(`pipeline_aggregators::aggregators()` + `curiosity_aggregators::aggregators()`).
Both must fold; second call must not replace first.

357 total (was 355).

### Net public-API delta: zero

No trait signatures changed, no methods added/removed, no behavior
on the healthy path changed. Operational hardening only.

## [0.3.4] - 2026-05-06

A Kurrent-alignment release. Two changes that move v0.3 closer to
the subscription model real event stores expose: an opt-in
`Fact::occurred_at` and a deprecation of `AnyMaterializer`.

### Changed (breaking, mechanical)

- **`Fact::occurred_at` now returns `Option<DateTime<Utc>>` with a
  `None` default.** Producer-claimed time stays expressible —
  override the method when your domain has backdating, batch
  historical import, or out-of-order arrival from external
  producers (Stripe webhooks, Slack delivery delay, mobile offline
  buffer). For domains where logical time *is* log time, leave the
  default and skip the carrier-event payload bloat.

  Runner-side fallback: `ctx.now()` resolves to
  `fact.occurred_at().unwrap_or(event.created_at)`. The
  `event.created_at` envelope timestamp is set by `Engine::emit_in`
  / `Engine::append_in` from `fact.occurred_at().unwrap_or_else(Utc::now)`
  at write time — same value either way for events emitted in this
  process; the difference matters only when externally-imported
  events flow through a backend's append API.

  **Migration for existing impls:** wrap returns in `Some(...)`.
  Trivial mechanical change; the `#[event(stream_category, stream_id)]`
  macro now emits the wrapped form automatically.

### Deprecated

- **`AnyMaterializer`, `AnyMaterializerRunner`,
  `EngineBuilder::with_any_materializer`** — marked
  `#[deprecated(since = "0.3.4")]`. The trait was a migration shim
  for legacy `Projection<D>` bodies that pattern-match across event
  types; it reads every event from the log without a declared
  subscription, which doesn't compose with Kurrent's stream/type
  subscription model.

  **Migration:** typed `Materializer<Fact = F>` consumers declare
  their subscription via `Fact::type_prefix()`, which maps to
  `$et-{prefix}` Kurrent subscriptions. Cross-domain consumers
  split into one typed materializer per consumed enum, all
  delegating to a shared body. ~15-line stubs each, no framework
  surface change required.

  Removal scheduled for 0.4.0. Existing call sites continue to
  compile and run unchanged through the 0.3.x line; they emit
  deprecation warnings as a migration nudge.

### Tests

`fact_without_occurred_at_falls_back_to_event_created_at` —
verifies the runner-side fallback when a Fact uses the trait
default. 355 total (was 354).

## [0.3.3] - 2026-05-06

Patch release restoring `ctx.aggregate()` access to v0.3 consumer
bodies. The accessor was cut from initial v0.3; this release brings
it back with **per-runner registry copies** (each consumer gets its
own `AggregatorRegistry` clone) so parallel runners no longer race
on shared folds.

### Added

- **`Ctx::aggregate::<A>() -> AggregateState<A>`** and
  **`Ctx::aggregate_of::<A>(id) -> AggregateState<A>`** —
  `(prev, curr)` snapshots around the per-event fold. `curr`
  reflects state INCLUDING the current event because the runner
  folds before invoking the consumer body. Same shape as 0.2.x; no
  `Option` wrapper. **Panics** with a clear message if no
  aggregators were registered — calling `aggregate()` without
  configuration is a programmer error worth surfacing loudly.

- **`EngineBuilder::with_aggregators<I: IntoIterator<Item = Aggregator>>(I) -> Self`** —
  chainable, accumulating. Mirrors the legacy
  `causal::Engine::with_aggregators` shape so call sites migrate
  unchanged. Typically fed by the `#[aggregators]` macro:
  ```rust
  EngineBuilder::new(...)
      .with_aggregators(my_aggs::aggregators())
      .with_aggregators(other_aggs::aggregators())
  ```

- **Per-runner `Aggregator` clones.** `Aggregator` now derives
  `Clone` (cheap — every non-trivial field is `Arc<dyn Fn>`). Each
  consumer's `AggregatorRegistry` is independent state with shared
  `Aggregator` definitions; folds in `ProjectionRunner`,
  `ReactorRunner`, `AnyMaterializerRunner` no longer contend.

- **Per-runner cold-start hydration.** On the first `step()` past
  cursor 0, each runner replays the log into its registry so
  consumers picking up at a non-zero cursor see correct
  pre-cursor state. Cost is one O(log size) sweep per runner
  lifetime; snapshot-based acceleration is a future enhancement.

### Behavior

- **Capture/restore around fold.** Each runner captures aggregator
  state before applying the event, then restores it if the consumer
  body returns `Err`. Mirrors the legacy engine's
  `capture_for_rollback` / `restore_state` discipline so retried
  events don't double-fold non-idempotent `Apply<E>` impls.

### Tests

3 new tests verifying: materializer `ctx.aggregate.curr` includes
the current event; reactor sees the `(prev, curr)` transition each
event; `ctx.aggregate` panics with the configured message when no
aggregators registered. Plus 3 previously-broken context tests that
now compile under the new struct layout. 351 total (was 345).

### Compatibility note for rootsignal

Shapes match the existing `causal::Engine` surface so consumers
migrating to v0.3 don't have to rewrite call sites:
- `ctx.aggregate::<X>().curr` — same accessor shape
- `.with_aggregators(my_mod::aggregators())` — same builder API
- `#[aggregators]` macro output drops in unchanged

## [0.3.2] - 2026-05-05

Patch release continuing the audit cleanup.

### Fixed

- **`commit_reactor_batch` now takes `Vec<InsertableOutboxRow>`** instead
  of `Vec<OutboxRow>`. The new type is the input shape — same fields
  minus `id` and `created_at`, which are backend-assigned. Eliminates
  the awkward "construct OutboxRow with placeholder values that get
  immediately overwritten" pattern. `OutboxRow` (the read-shape with
  `id` + `created_at`) remains the return type of `outbox_pending`.

  Breaking change for any external `ReactorOutbox` implementor — the
  trait method signature changed. Internal-only types (`MemoryStore`)
  updated. Pre-1.0 SemVer allows this in patch releases; the change
  is strictly an ergonomic improvement.

### Added

- **2 new tests** verifying correlation_id propagation through the
  reactor → outbox → relay → log chain, and Materializer ctx
  carrying the persisted event's correlation_id (not regenerated).
  Previously-untested invariants now made explicit.

### Documentation

- **C11 honest restatement.** Framework does NOT structurally enforce
  that Reactor outputs target only OpenAppend streams. If a Reactor
  emits a fact whose `stream().category` was registered as
  OCC-required, the relay drains it unchallenged. C11 is a consumer-
  side discipline; future enhancement could register category-per-
  output-event-type at builder time so the runner rejects pre-commit.

- **C13 honest restatement.** Framework cannot structurally enforce
  purity in `materialize()` bodies. `Ctx` exposes a narrow surface
  but consumer bodies can still query foreign state and produce
  non-deterministic output on replay. C13 is a discipline backed by
  `Ctx`'s narrowness, not a guarantee.

## [0.3.1] - 2026-05-05

Patch release fixing three audit-flagged issues from 0.3.0. All
changes are additive; existing 0.3.0 callers continue to work.

### Fixed

- **`Engine::emit_in` and `Engine::append_in`** added — caller-supplied
  `correlation_id`, `parent_id`, and `metadata` for command-handler use
  cases. The existing `emit` / `append` methods are now thin wrappers
  over `*_in` with `WriteOptions::default()` (auto-generated
  correlation_id, no parent, empty metadata). Critical for any consumer
  responding to upstream requests where causal-chain tracing matters.

- **`Engine::await_observed_by` signature simplified.** Was:
  `await_observed_by(&self, checkpoint: &Arc<dyn CheckpointStore>, id, pos)`.
  Now: `await_observed_by(&self, id, pos)`. The engine stores its
  checkpoint store internally (added a field), so callers don't need to
  pass it. **This is a breaking change** to the 0.3.0 method signature
  — but 0.3.0 was published just hours before 0.3.1, and the previous
  signature was an oversight.

- **`derive_output_event_id` uses NUL-byte separator** instead of pipe
  delimiter. Robustness against pathological reactor_ids that contain
  the separator character. UUIDs as deterministic outputs of
  `(reactor_id, trigger_event_id, output_index)` are now unambiguous
  regardless of reactor_id contents.

### Tests

`Engine::emit_in` correlation propagation, parent + metadata
propagation, `append_in` per-batch correlation propagation, and
`await_observed_by` without checkpoint param — 4 new tests; 345 total.

## [0.3.0] - 2026-05-05

A redesign that adds a database-agnostic, ES-aligned, KurrentDB-compatible
trait surface alongside the existing 0.2.x API. All 0.2.x code continues
to work unchanged; v0.3 is additive.

### New traits (application-facing)

- **`Fact`** — value-level event trait with `type_name`, `type_prefix`,
  `occurred_at` (logical clock), and `stream` (mandatory `StreamRef` for
  Kurrent compatibility). The `#[event]` macro now optionally generates
  `impl Fact` when `stream_category` and `stream_id` attributes are
  supplied.
- **`Aggregate`** (at `crate::aggregate_v3::Aggregate`) — write-side
  consistency boundary with single-Fact-per-aggregate, `apply` on the
  trait directly. Coexists with the legacy `crate::aggregator::Aggregate`
  until removal in a future release.
- **`Materializer`** — typed external-state writer. Idempotent at-least-once
  delivery; runtime calls `materialize(fact, ctx)` per fact.
- **`AnyMaterializer`** — heterogeneous-event consumer (sees every event,
  takes `PersistedEvent` directly). Migration target for legacy
  `Projection<D>` bodies that pattern-match across multiple event types.
- **`Reactor`** (at `crate::reactor_v3::Reactor`) — pure decision
  producing `Events`. Forward-only; outputs go through the runtime-side
  outbox. Coexists with the legacy `crate::reactor::Reactor<D>` builder
  struct.

### New backend traits

- **`EventLogBackend`** — splits `EventLog` minus the snapshot methods,
  adds `append_to_stream` (CAS-protected aggregate writes). Default impl
  forwards to existing methods (non-atomic); `MemoryStore` overrides
  with single-mutex CAS.
- **`CheckpointStore`** — minimal per-consumer cursor (`get`/`set`).
  Blanket-implemented for any `ProjectionStore`.
- **`ReactorOutbox`** (extends `CheckpointStore`) — atomic batch primitive
  (`commit_reactor_batch`, `outbox_pending`, `outbox_delete`) for runtime-
  side reactor outbox per the C12 contract.
- **`SnapshotStore`** — extracted snapshot read/write. Blanket-implemented
  for any `EventLog`.

### New runtime

- **`Engine` + `EngineBuilder`** at `crate::engine_v3` — the v0.3 engine
  driving Materializers, Reactors, and AnyMaterializers via per-consumer
  supervisor tasks plus a relay loop draining the reactor outbox into
  the log.
- **`ProjectionRunner<M>`**, **`ReactorRunner<R>`**, **`AnyMaterializerRunner<M>`** —
  per-consumer runners with per-fact cursor advance (C2), `DEPENDS_ON`
  fence (C2b), and BlockUntilFixed failure semantics.
- **`RelayLoop`** — drains reactor outbox via at-least-once delivery
  with deterministic `event_id` (uuid v5 over `(reactor_id, trigger_id,
  output_index)`) so retried reactor runs collapse into one log entry
  via the log's idempotent-append-on-event-id contract.

### Single context type

- **`Ctx<'a>`** (at `crate::contexts::Ctx`) — passed to every consumer
  body. Exposes `event_id`, `log_position`, `occurred_at`, `correlation_id`,
  `metadata`. Critically, **no wall-clock accessor**: `ctx.now()` returns
  `occurred_at`. Replay reproduces byte-identical state.

### Macro extensions

- `#[event(prefix = "...", stream_category = "...", stream_id = "...")]`
  now additionally generates `impl Fact` when both `stream_category` and
  `stream_id` are supplied. Per-variant `occurred_at` field expected by
  default; override with `occurred_at_field = "..."`.

### Runtime contracts

13 contracts documented in the `2026-05-05-causal-v03-api-design-plan.md`
design doc (see repository). Verified via 341 tests including
crash-injection covering C2 (per-fact cursor advance) and C12
(atomic outbox + cursor commit) under failure.

### Known limitations

- **BS1 partial closure:** `Engine::emit` rejects writes to streams
  registered as OCC-required (via `with_aggregate<A>`), but unregistered
  streams default to permissive. Closing BS1 fully needs a future release
  that flips the default to "reject unregistered."
- **No `#[materializer]` macro yet.** Materializer impls are hand-rolled
  for now; a builder-style macro is planned for 0.3.x.
- **Postgres backend optimizations app-side.** The framework provides
  `EventLogBackend::append_to_stream` with a non-atomic default; backends
  with native CAS primitives (Postgres `SELECT FOR UPDATE`) should
  override. `ReactorOutbox` and `CheckpointStore` impls for Postgres
  are application-side until upstream `causal_replay` adds reference
  impls.

### Backward compatibility

- All 0.2.x traits and APIs continue to work unchanged.
- Legacy `Engine<D>`, `Projection<D>`, `Reactor<D>` builder struct,
  `aggregator::Aggregate`, `reactor::Context<D>` all coexist with the
  v0.3 surface.
- Existing `#[event]` enums without stream attributes generate only
  `impl Event`, not `impl Fact`. No breaking change.
- Removal of legacy traits planned for a future major release.

## [0.2.2] - 2026-05-04

### Added

- New module `causal::projection` exposing the public API surface for
  the upcoming async-projection runtime (target release 0.3.0). Backend
  implementers can develop a `ProjectionStore` impl against these types
  in parallel with the engine-side runtime work.
- `ProjectionMode` enum (Sync, Async) — explicit mode declaration, no
  default. Rationale in
  `docs/plans/2026-05-04-feat-async-projections-plan.md` (D1).
- `RetryPolicy` with `Backoff` (Linear, Exponential with jitter) and
  `FailureBehavior` (BlockUntilFixed default, AdvanceAfter opt-in).
- `StartPosition` enum (ResumeOrLatest, Latest, Zero, Specific) — no
  default. Forces explicit backfill decision at registration time (D5).
- `ProjectionStatus` and `ProjectionFailure` row types for status
  queries and DLQ inspection.
- `ProjectionStore` trait with CAS semantics (`expected_from`
  parameter, `Result<bool>` return) and atomicity contract on
  `advance_past_failure` (DLQ write + cursor advance in one
  transaction, CAS check first). Twelve methods covering registration,
  cursor management, status reporting, ops (pause/resume/reset),
  deletion, and DLQ listing.
- `MemoryStore` implements `ProjectionStore`, including correct lock
  ordering and CAS-on-DashMap-entry-write-lock semantics.
- 35 conformance tests for the trait at
  `modules/causal/tests/projection_store.rs`. Includes 4 multi-threaded
  tests verifying CAS correctness under concurrent access (the
  load-bearing tests for multi-process backend correctness).

### Status: API stub, no runtime yet

The runtime that consumes these types (engine integration,
`ProjectionRunner`, `Engine::register_projection` and friends) is not
implemented in this release. Async projections cannot yet be registered
against a live engine — they will be in 0.3.0. This release ships the
type surface so backend implementers (notably the Postgres
`ProjectionStore` consumer-side impl) can develop in parallel.

Sync projections continue to use the existing `engine.with_projection(...)`
path with the failure semantics from 0.2.1 (cursor blocks on failure,
event parks after retry budget).

### Yanked

`causal` v0.2.0 (and the rest of the workspace at 0.2.0) was yanked
from crates.io after the aggregator double-apply correctness regression
was discovered and fixed in 0.2.1. Existing pinned consumers are
unaffected; new resolutions skip 0.2.0.

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
