---
title: "feat: Async projections with per-projection cursors"
type: feat
date: 2026-05-04
status: accepted-design-pending
target_release: 0.3.0
estimate: ~5 weeks (engine 2-3w + consumer migration 1w + testing 1w)
---

# Async projections with per-projection cursors — ACCEPTED, design pending

## Status

**ACCEPTED, design pending.** Position flipped 2026-05-04 after a second
pressure-test from a consumer-side review. The original rejection (deferring
to `causal_replay::ProjectionStream`) was correct on the principle of
"single concern per crate" but applied along the wrong axis. The right
split is **dispatch-cursor (engine) vs replay-mode + promote-gates
(`causal_replay`)** — not sync vs async. Per-projection async cursors are
a runtime concern that belongs in the engine because that's where
projection registration lives. Forcing async-mode users into a separate
crate makes the dangerous mode (coupled-to-dispatch via
`engine.with_projection`) the easy default, and the safe mode the one
behind a doc pointer. That's the wrong API shape.

This document captures the accepted direction. Three design decisions are
resolved (below); two remain open. Implementation does not start until the
design doc lands.

## Position history

1. **First proposal (this document, original version).** Add
   `ProjectionMode::Sync | Async` to existing `Projection<D>`. Engine spawns
   `ProjectionRunner` per async projection.
2. **Pressure-test → rejection.** Found `causal_replay::ProjectionStream`
   already exists with most of the desired runtime behavior. Concluded
   adding async mode to the engine was redundant.
3. **Consumer-side counter-pressure-test.** Consumer pointed out that
   `causal_replay::ProjectionStream` does NOT cover engine-native
   registration ergonomics, pause/resume/reset ops, structured per-projection
   DLQ, or unified API. Forcing users to a different crate for the safe
   mode makes the dangerous mode the default.
4. **Position flipped to accepted.** Engine-native projection registration
   with explicit Sync/Async mode, multiple ergonomic constructors, and
   per-projection cursors managed by the engine. `causal_replay` pivots to
   `REPLAY=1` rebuild + promote-gate use case (its actual differentiator).

## Resolved design decisions

These are settled. Don't re-litigate without strong new evidence.

### D1: Mode selection — explicit, no default

Refuse to pick a default. Every `register_projection` call site declares
its mode. Both API shapes available:

```rust
// Explicit-arg form
engine.register_projection("schedule_state", projection_fn, ProjectionMode::Sync);
engine.register_projection("search_index",   projection_fn, ProjectionMode::Async);

// Named-constructor form (sugar over the above)
engine.register_sync_projection("schedule_state", projection_fn);
engine.register_async_projection("search_index", projection_fn);
```

Why neither default works: defaulting Sync preserves existing buggy
coupling for new projections; defaulting Async silently breaks existing
read-your-writes consumers. Refusing to default forces a per-site
deliberate choice. Existing call sites have to be touched anyway because
the trait shape changes; the migration is the natural moment for the
explicit choice.

### D2: `settle()` semantics — sync-only

`engine.emit(x).settled().await` waits for sync projections to catch up
to event x's position. Async projections are NOT awaited. For async
projection observation, callers use:

```rust
engine.wait_for_projection("search_index", target_position).await?;
engine.projection_position("search_index").await?; // for polling
```

This is the third option from the prior pushback ("sync only, expose
`wait_for_projection` separately"). Cleanest separation: `settle()`
keeps its current contract (covering the synchronous causal tree); async
projections opt into their own observation API.

### D3: Multi-process leader election — per-batch SKIP LOCKED, defer leases

The 0.3 design assumes single-active-runner-per-projection, claimed
per-batch via `FOR UPDATE SKIP LOCKED` on the cursor row. No long-lived
leases, no heartbeat protocol, no fencing tokens. This is sufficient for
single-process deployments and forwards-compatible with future multi-
process work.

Forward-compatibility commitment: the `causal_projection_cursors` table
will include `leased_by`, `leased_until`, `fencing_token` columns
unused-but-present in 0.3 so that leases can be added in a later release
without a schema redesign — just column population.

Multi-process scaling (long-lived leases, heartbeat protocol, region-
aware leader election) is deferred to a separate RFC. Don't pre-build it.

## Open design questions

These still need answers before implementation starts.

### Q1: Per-projection retry policy granularity

The original proposal had per-projection `RetryPolicy { max_attempts,
backoff }`. Sensible default: `Exponential { base: 100ms, max: 30s,
jitter: true }`, max_attempts unbounded for async (catches up eventually)
or some finite number that triggers DLQ (per-projection failure stream).

Question: is the DLQ for async projections (a) per-projection failure
stream, indefinite retry above it, or (b) finite attempts then move-on
(advance cursor past the failed event)?

Marten goes with (a): retry forever, surface lag. EventStoreDB allows
either via the `subscription` config. Recommend (a) for async + structured
per-projection DLQ rows for failed-events visibility, but explicit decision
needed before traits land.

### Q2: Backfill-vs-skip on registration

When `register_projection` is called for a projection_id whose cursor
already exists, two valid behaviors:

- Use existing cursor. Caller intent: "resume where I left off."
- Reset to `Latest` or `Zero`. Caller intent: "fresh start" or "backfill."

Proposal: registration takes a `start_position: StartPosition` enum:

```rust
pub enum StartPosition {
    /// Use existing cursor if present, else Latest. Sane default for prod.
    ResumeOrLatest,
    /// Always start at current dispatch position. New projections, no backfill.
    Latest,
    /// Always start at LogCursor::ZERO. Force backfill from beginning.
    Zero,
    /// Specific position. Manual rewind.
    Specific(LogCursor),
}
```

Default `ResumeOrLatest`. Forces caller to think about backfill the moment
they add a projection that needs historical state.

This is the answer to the migration footgun the consumer flagged. Without
this, "first boot of new code initializes cursor at current dispatch
position" silently makes historical events invisible to new projections.

## Audit deliverable (consumer-side, prerequisite to migration)

Before consumer migration starts, produce a table with three columns per
projection:

| projection_id | downstream_consumers | required_mode |
|---------------|----------------------|---------------|
| schedule_state | runner.rs read of next_run_at within dispatch | Sync |
| search_index   | (none — only queried by HTTP API)             | Async |
| ...            | ...                                           | ...   |

The "downstream_consumers" column is what makes mode selection concrete —
the question isn't "does this projection look async-friendly?" it's "is
anything reading the state synchronously within the same dispatch cycle?"
That answer comes from the consumer side, not the projection side.

Output goes into the consumer's migration PR description.

## Implementation plan (high-level)

Estimate ~5 weeks. Breakdown:

**Engine work (2-3 weeks):**
- `ProjectionStore` trait + `MemoryStore` impl (+ `causal_projection_cursors`
  schema for backend impls). Includes the forward-compat columns from D3.
- `ProjectionRunner` with retry policy, exponential backoff, per-batch claim.
- Engine spawns/cancels async runners during `settle()` lifecycle.
- Sync mode integration: sync projections still run in
  `process_event_inner` like today's 0.2.1, but registered through the
  new unified API.
- `register_projection` / `register_sync_projection` / `register_async_projection` API.
- `engine.wait_for_projection(id, target)` and `engine.projection_position(id)`.
- `engine.pause_projection(id)` / `resume_projection` / `reset_projection`.
- Per-projection structured DLQ row format.

**Consumer migration (~1 week):**
- Implement `ProjectionStore` for the consumer's Postgres backend.
- Run audit (above table) and update each `register_projection` site.
- Schema migration: `causal_projection_cursors` table.

**Testing + rollout (~1 week):**
- Catch-up correctness, restart resume, lease-takeover (when leases land
  later), backfill via `StartPosition::Zero`.
- Inspector UI panel for per-projection cursor / lag / pause-resume.
- Staged rollout with monitoring on parked-event count and per-projection
  lag metrics.

---

# Historical context (rejection-era content, preserved for reference)

The sections below are from when this plan was in `rejected` status. They
document the pressure-test reasoning that led to the original rejection
and the question-by-question comparison with `causal_replay`. Useful
background but not normative — the resolved decisions above are what
the implementation will follow.

## What the original plan proposed

Add an opt-in `ProjectionMode::Async` to the existing `Projection<D>` type.
Async projections would:

- Have their own cursor (independent of dispatch cursor)
- Run in a `ProjectionRunner` worker spawned by `Engine::settle`
- Retry indefinitely with backoff on failure
- Not block dispatch
- Get JSON-only event payloads (no ephemerals)

This required:

- Trait extensions to `ReactorQueue` (per-projection cursors, lease-based
  leader election)
- `ProjectionRunner<D>` struct integrated into the engine
- Sync/async API on `Projection` (`mode(...)` builder method)
- New observation API (`Engine::projection_position`, `wait_for_projection`)
- Migration story for existing projections

## Why rejected

The library already has an async projection primitive — `ProjectionStream` in
the `causal_replay` crate (`modules/causal_replay/`). It's:

- Outside the engine (separate crate, separate concerns)
- Backed by its own cursor (`PointerStore` trait + `PgPointerStore`)
- Runs catch-up + tail in live mode, full replay + promote in replay mode
- Transport-agnostic (`TailSource` trait abstracts PG NOTIFY / polling)
- Designed to coexist with inline `project().then()` — not replace it

The design brainstorm at `docs/brainstorms/2026-03-09-replay-projection-library.md`
(status: implemented) explicitly addresses the same question this plan was
asking, and arrives at the opposite architecture: **two primitives in two
crates, not one type with a mode toggle.**

The two-primitive split is structurally cleaner than the mode-toggle approach:

1. **Engine stays single-purpose.** No leader-election hook in
   `ReactorQueue`. No async-projection-aware code in the settle loop.
2. **`settle()` keeps a clean contract.** `engine.emit().settled().await`
   covers the inline path only. No "settle is a partial promise" problem.
3. **Replay mode comes for free.** `ProjectionStream::Mode::Replay` already
   handles full rebuilds with active/staged pointers and promote gates.
   The mode-toggle approach had no story for this.
4. **Multi-process coordination lives in `PointerStore` impls.** Advisory
   locks (or whatever) belong in the persistence backend, not the engine
   trait.

## Mapping the original design questions

| Q  | Topic | This plan's answer | Library's existing answer |
|----|-------|--------------------|---------------------------|
| Q1 | Concurrency / leader election | Lease-based via `ReactorQueue` extension | `PointerStore` impl-specific (advisory locks in `PgPointerStore`, etc.) |
| Q2 | Ephemerals | JSON-only | `ProjectionStream` is `EventLog`-driven → JSON-only by construction |
| Q3 | Observation API | New `Engine::projection_position` + `wait_for_projection` | `PointerStore::status() -> { active, staged }`; lag = `latest_position - active` |
| Q4 | Failure semantics | Unbounded retry with backoff | Replay = fail-fast; live = log-and-continue (explicit design choice in the brainstorm) |
| Q5 | Migration | Cursor starts at current dispatch position | `PointerStore.set(0)` for full backfill; defaults to `active` |

Every question has an answer that already lives in `causal_replay`. Adding
the mode-toggle would re-build all five inside the engine for no
architectural gain.

## What this means for callers

If you have a sync projection that should not block dispatch on failure, the
answer is **not** "wait for async projection mode." The answer is one of:

1. **Use `causal_replay::ProjectionStream`** for read models that tolerate
   lag (search indexes, derived views, blue-green rebuild targets). Documented
   pattern, idempotent contract, transport-agnostic.

2. **Use a regular reactor with `on_any().background().retry(...)`** for
   per-event side effects that should retry without blocking dispatch. You
   lose strict log-order processing but gain the standard reactor DLQ
   semantics.

3. **Keep using sync `project().then()`** if you actually need read-your-writes
   inside a single causal chain. Failure now blocks dispatch and parks the
   event after retries — that's the new (correct) semantics from the v0.2.0
   fix.

## Possible follow-ups in `causal_replay` (not this plan)

If `causal_replay` is missing ergonomics that consumers actually need, file
them as enhancements there. Candidates noticed during this pressure-test:

- **Lag metric helper.** Today: `latest_position - pointer.status().active`.
  Could be wrapped as `ProjectionStream::lag()` for one-line consumer use.
- **Multi-process leader election in `PgPointerStore`.** Advisory-lock-based
  claim so multiple `server` processes don't redundantly apply each event.
  Wasteful (idempotent) rather than corrupt today, but worth fixing for
  high-volume deployments.
- **Per-event retry/backoff option in live mode.** Today's "log and continue"
  is one explicit choice; a second choice ("retry N times with backoff before
  log-and-continue") would let consumers protect transient external services
  without rewriting `apply`.

These are `causal_replay` issues, not engine issues, and not this plan.

## References

- `modules/causal_replay/` — the existing async projection primitive.
- `docs/brainstorms/2026-03-09-replay-projection-library.md` — the
  pressure-tested design that resolves the same question this plan was
  asking. Read this instead.
- `docs/plans/2026-05-04-fix-projection-failure-cursor-advance-plan.md` —
  the v0.2.0 sync projection fix. Self-contained; doesn't depend on this
  plan.
