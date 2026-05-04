---
title: "feat: Async projections with per-projection cursors"
type: feat
date: 2026-05-04
status: deferred
prereq: 2026-05-04-fix-projection-failure-cursor-advance-plan.md
---

# Async projections with per-projection cursors

## Status

**DEFERRED.** This plan is intentionally not being implemented in the same PR as
the projection-failure-cursor-advance fix. It needs design work that doesn't fit
inside a tactical bug-fix scope. Captured here so the next person picking it up
has the full context.

## Overview

Add an opt-in `Async` projection mode where projections process the event log
through their own independent cursor instead of running inline in the dispatch
loop. Sync remains the default and matches today's semantics (after the
projection-failure fix lands).

Two abstractions, two failure modes:

- **Sync projections** (default, today's behavior plus the bug fix): run inline
  before reactor dispatch. Failure blocks dispatch — the dispatch cursor doesn't
  advance until the projection succeeds (or the event-retry budget exhausts and
  the event parks).
- **Async projections** (new): run in a dedicated runner against their own
  cursor. Failure retries that projection only, with backoff. Doesn't block the
  dispatch cursor or other projections. Useful for read models that tolerate
  lag (search indexes, analytics aggregations, etc.).

## Problem statement

The projection-failure-cursor-advance fix closes the silent-loss bug, but it
leaves projections as a single rigid abstraction:

1. **All projections are critical-path.** A flaky search-index projection can
   stall reactor dispatch the same way a load-bearing schedule-state projection
   would. The failure model is one-size-fits-all.

2. **No way to express "this read model can lag."** Every projection blocks the
   dispatch cursor. Consumers have no escape hatch when a projection failure is
   acceptable to defer.

3. **No per-projection observability.** A "projection lag" metric is impossible
   to compute when projections share the dispatch cursor. The system knows "the
   engine is N events behind" but can't say "the search-index projection is N
   events behind, the schedule-state projection is current."

These aren't urgent. The bug fix is. But once the bug is closed, this is the
next layer of design pressure.

## Why this is not Track A

Bundling this with the bug fix was over-scoped. The plumbing (per-projection
cursors, ProjectionRunner) is mechanical. The API design isn't:

1. **Concurrency model.** A `ProjectionRunner` per async projection is fine in
   single-process MemoryStore. In a multi-process Postgres backend (the actual
   consumer shape), two engine instances both run their own runner for the same
   projection and both apply event N. Needs leader election or `SKIP LOCKED`
   claim-and-advance — the same machinery the prior outbox plan was building
   for the publisher loop. Not a half-day.

2. **Ephemeral handling.** The engine's in-memory ephemeral cache
   (`engine.rs:391-394`) injects the originally-typed event into Phase 2 so
   projections holding `#[serde(skip)]` fields work. An async runner reading
   from a cursor doesn't see that cache (different code path, possibly
   different process). Either ephemerals get serialized (defeating their
   purpose) or async projections get JSON-only. Real API decision.

3. **`settle()` observation.** Today `engine.emit().settled()` waits for the
   entire causal tree, including projections. Async projections decouple from
   that wait. Tests and scripts that assert projection state after `settled()`
   silently fail. There's no API for "wait until projection X catches up to
   position N." Needs design.

4. **Backwards compatibility.** Existing callers must not be broken. Default
   must remain Sync. The opt-in API needs to feel native, not bolted on.

These four problems are individually tractable and collectively a deliberate
piece of work. Doing them under time pressure inside a bug fix produces shaky
abstractions.

## Open design questions

These need answers before implementation starts.

### Q1: Concurrency model for async projection runners

**Options:**

- **A.** One tokio task per async projection, owned by the engine. Single-
  process only. Trivial.
- **B.** Engine claims projection slots via `ReactorQueue::claim_projection`
  (advisory lock or `SKIP LOCKED` row), runs that projection's runner, releases
  on shutdown. Multi-process safe.
- **C.** External coordination: leave runner ownership to the deployer
  (sidecar process, k8s job, etc.). Engine just exposes the runner as a
  callable; deployer ensures only one runs.

**Recommendation:** B for production correctness, with A as a fallback when the
queue impl returns `None` from `claim_projection` (MemoryStore single-process
case). C is a cop-out — pushes the hard problem to users.

### Q2: Ephemeral handling for async projections

**Options:**

- **A.** Async projections get JSON-only event payloads. `#[serde(skip)]`
  fields are unavailable. Document the limitation.
- **B.** Engine serializes ephemerals into `PersistedEvent` so they survive
  the trip through the log. Defeats the purpose of ephemerals (they exist
  to avoid serialization for in-process events).
- **C.** Engine maintains a TTL-bounded ephemeral cache. Async runners check
  the cache first, fall back to JSON-only after expiry. Best of both worlds
  but adds memory pressure and cache-coherence complexity.

**Recommendation:** A. Document loudly. Async projections are for read-model
work that's already JSON-derivable. If a projection needs ephemeral fields, it
should be Sync. The mode choice naturally encodes the constraint.

### Q3: Observation API for async projections

How do callers wait for an async projection to catch up?

```rust
engine.emit(event).settled().await?;
engine.projection_caught_up("search_index").await?; // wait for async to catch up
// now safe to assert state
```

**Options:**

- **A.** `Engine::projection_position(id) -> LogCursor` — caller polls.
- **B.** `Engine::wait_for_projection(id, position).await` — blocks until cursor
  passes the given position.
- **C.** `engine.emit(...).settled_with_projections(["search_index"])` —
  builder-style explicit waits, integrated with `EmitFuture`.

**Recommendation:** A and B. C is sugar that can be added later. `wait_for_*`
is the primitive tests will reach for.

### Q4: Failure semantics for async projections

If an async projection fails, what happens?

- Dispatch cursor advances (it doesn't depend on this projection).
- That projection's cursor stays put.
- Retry with backoff. No max attempts — keep retrying forever, since async
  projections are idempotent replay; the bug or transient cause will be fixed
  externally.
- Surface lag via metric. Optionally emit a `ProjectionLagging { id, lag }`
  event past a threshold.

This matches how Kafka Streams and EventStore behave. No park, no DLQ — just
unbounded retry with visibility.

### Q5: Migration story

For consumers upgrading: existing projections default to `Sync`. No behavior
change beyond the Track A bug fix. Opt-in to `Async` per projection.

For the projection cursor: when a consumer first registers an async projection,
its cursor starts at the current dispatch cursor (i.e., it doesn't backfill
historical events). If the consumer wants backfill, they explicitly reset the
cursor to `LogCursor::ZERO` before registering. Document this; don't make it
implicit.

## Proposed API sketch

```rust
pub enum ProjectionMode {
    /// Default. Runs inline in the dispatch loop. Failure blocks dispatch.
    Sync,
    /// Runs in a dedicated runner with its own cursor. Failure doesn't block.
    Async,
}

impl<D> ProjectionBuilder<D> {
    pub fn mode(self, mode: ProjectionMode) -> Self;
}

// Usage
engine
    .with_projection(
        project("schedule_state")
            // .mode(ProjectionMode::Sync) — default, can omit
            .then(|event, ctx| async move { /* ... */ }),
    )
    .with_projection(
        project("search_index")
            .mode(ProjectionMode::Async)
            .then(|event, ctx| async move { /* ... */ }),
    );

// Observation
let pos = engine.projection_position("search_index").await?;
let lag = engine.latest_position().await? - pos;

engine.wait_for_projection("search_index", target_pos).await?;
```

## Trait extensions

```rust
#[async_trait]
pub trait ReactorQueue: Send + Sync {
    // ... existing methods ...

    /// Get the current cursor for an async projection.
    async fn get_projection_cursor(&self, id: &str) -> Result<LogCursor>;

    /// Advance an async projection's cursor.
    async fn advance_projection_cursor(&self, id: &str, position: LogCursor) -> Result<()>;

    /// Try to claim leadership for an async projection runner.
    /// Returns Some(token) if claim succeeded, None if another instance owns it.
    /// MemoryStore: always returns Some (single-process).
    async fn claim_projection(&self, id: &str) -> Result<Option<ProjectionLease>>;

    /// Renew a lease (heartbeat). Called periodically by the runner.
    async fn renew_projection_lease(&self, lease: &ProjectionLease) -> Result<()>;

    /// Release a lease on shutdown.
    async fn release_projection_lease(&self, lease: ProjectionLease) -> Result<()>;
}
```

## Schema changes (Postgres consumer)

```sql
CREATE TABLE causal_projection_cursors (
    projection_id TEXT PRIMARY KEY,
    cursor BIGINT NOT NULL,
    leased_by TEXT,
    leased_until TIMESTAMPTZ,
    last_error TEXT,
    last_error_at TIMESTAMPTZ
);
```

Lease columns enable Q1 option B (claim + heartbeat). `last_error` columns
enable Q4 visibility without infrastructure.

## Out of scope

- **Per-event-per-projection success ledger.** Tracked separately. Solves the
  retry-amplification of Track A and partially overlaps with this work, but
  it's a different correctness concern and shouldn't be conflated with the
  sync/async API.
- **Projection composition / dependency declaration.** "Projection B depends on
  projection A having advanced past position N." Real need for some consumers,
  but this design supports it via `wait_for_projection` polling. Native
  declaration is a future enhancement.
- **Cross-projection transactions.** "Update projection A and B atomically in
  one tx." This is incompatible with independent cursors and is the wrong
  pattern; if you need atomicity, use a single projection.

## Definition of done

- `ProjectionMode` enum exists, defaults to Sync, opt-in API works.
- Async projections have their own cursor, persisted via `ReactorQueue`.
- `ProjectionRunner` is implemented and integrated into `Engine::settle()`
  (only awaits sync projections).
- Leader election prevents duplicate processing in multi-process setups
  (Q1 option B).
- `Engine::projection_position(id)` and `Engine::wait_for_projection(id, pos)`
  observation APIs exist.
- Async projections survive backend restarts (cursor and lease persist).
- Failure mode: async projection failure increments retry counter, backs off,
  retries indefinitely; surfaced via `last_error` field. No park.
- Migration: existing consumers see no behavior change unless they opt in.
- Tests cover: catch-up after restart, lease takeover, failure backoff,
  observation timing, JSON-only ephemeral handling.
- CHANGELOG entry distinguishes this from Track A clearly.

## Estimated cost

- API + trait + MemoryStore: ~1 day.
- Postgres impl in consumer (out of this repo, but blocks consumer adoption):
  ~1 day.
- ProjectionRunner + leader election + tests: ~1-2 days.
- Documentation, migration guide, examples: ~0.5 day.

Realistic total: 3-4 engineering days for the in-tree work, plus consumer-
side schema and testing. Calling this "1-2 days" was the original sin of
the combined plan.

## References

- `engine.rs:391-394` — ephemeral cache that async runners can't see directly.
- `engine.rs:438-453` — event-retry counter (sync-only mechanism).
- `job_executor.rs:236-285` — current inline projection loop. The Sync path
  after Track A.
- `memory_store.rs:323-329` — current swallow-and-advance bug. Track A removes
  this; this plan ensures the slot stays empty.
- `docs/plans/2026-03-08-refactor-store-trait-split-plan.md` — prior trait
  split that this builds on.
- `docs/CLAUDE.md` — projection mental model: "observers, run before reactors,
  return `Result<()>`." Async mode is a deliberate exception to "run before
  reactors" with explicit opt-in.
