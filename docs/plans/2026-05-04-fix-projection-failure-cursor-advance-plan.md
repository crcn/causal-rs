---
title: "fix: Projection failure must not advance the dispatch cursor"
type: fix
date: 2026-05-04
breaking: true
followup: 2026-05-04-feat-async-projections-plan.md
---

# Projection failure must not advance the dispatch cursor

## Overview

When a projection returns `Err` during event processing, the engine currently
records the failure into `IntentCommit::projection_failures`, returns the
commit successfully, and advances the dispatch cursor anyway. The failure is
logged via `eprintln!` and discarded. The event is treated as "processed"
even though its derived state was never written.

This PR closes that hole: a projection failure causes `process_event_inner` to
return `Err`, the existing event-retry budget retries the event, and after
`max_event_retry_attempts` the event parks with reason `projection_failed:<id>`.
No silent loss.

This is a breaking change for any consumer relying on the silent-advance
behavior. Bumps the crate to `0.2.0`.

## Problem statement

`job_executor.rs:236-285` runs projections inline. When one returns `Err`:

```rust
if let Err(error) = (projection.reactor)(any_event, ctx).await {
    warn!(...);
    projection_failures.push(ProjectionFailure { ... });
}
// loop continues — other projections still run
// ...
Ok(IntentCommit { ..., projection_failures, checkpoint: event.position })
```

The engine then calls `queue.enqueue(commit)` (`engine.rs:481-484`). The
trait contract says enqueue "atomically persists reactor intents AND advances
the checkpoint." `MemoryStore::enqueue:323-329` handles `projection_failures`:

```rust
for failure in commit.projection_failures {
    eprintln!("Projection DLQ: ...");
}
// ...
self.checkpoint.store(commit.checkpoint.raw(), Ordering::SeqCst);
```

→ Failure is printed and discarded. Cursor advances. Event is gone forever.

For an event-sourced consumer that uses projections to maintain queryable
derived state (e.g., `UPDATE schedules SET next_run_at = ...` in response to
`ScheduleTriggered`), this means: when the projection fails, the event is
durable but the state isn't, and no retry happens. The next dispatch cycle
fires the same schedule against the stale `next_run_at`. Silent double-fire.

## Root cause

`IntentCommit` was carrying two concerns: (a) atomic enqueue + cursor
advancement, and (b) projection failure reporting. When one type does two
jobs, callers can drop one of them. `MemoryStore::enqueue` did exactly that.
The bug is structural: as long as the type carries failure data, every
backend implementer can re-introduce the silent-advance behavior.

## Proposed solution

Three changes, all mechanical once the design is agreed:

1. **Remove `ProjectionFailure` and `IntentCommit::projection_failures`.**
   The type returns to one job: atomically enqueue intents and advance the
   dispatch cursor.

2. **Make `process_event_inner` return `Err` when any projection fails.**
   Run all projections per attempt (preserving the existing parallel-
   independent-projection semantics), collect any errors, return
   `Err(anyhow!("projection {} failed: {}; ..."))` after the loop if any
   failed. The engine's existing retry path at `engine.rs:485-489` already
   handles this — `break`s the inner loop, retries on the next iteration,
   parks the event after `max_event_retry_attempts` (default 3).

3. **Drop the `eprintln!`-and-discard block in `MemoryStore::enqueue`.**
   With no `projection_failures` field on `IntentCommit`, the block has
   nothing to drain. Remove it.

That's the entire fix.

## Behavior after the fix

**Single projection, fails once then succeeds:**
- Attempt 1: projection fails. `process_event_inner` returns `Err`. Cursor
  doesn't advance.
- Attempt 2: projection succeeds. Cursor advances. Reactors fire normally.

**Single projection, fails permanently:**
- Attempts 1, 2, 3: projection fails. Counter reaches `max_event_retry_attempts`.
- Attempt 4: event parks with reason `Event failed after 3 retry attempts`.
  Cursor advances past it. Reactors never fired for this event.

**Two projections (priority A=1, B=2), A fails, B succeeds:**
- Attempt 1: A fails, B runs and succeeds. Loop returns `Err`. Cursor doesn't
  advance.
- Attempt 2: A runs again (must be idempotent — pre-existing contract), B runs
  again (must be idempotent). If A succeeds, cursor advances.

This last case is the controversial one. The existing test
`projection_error_does_not_stop_other_handlers` asserts "reactor fires
even when projection errors." That test was load-bearing for the bug. After
this fix, the reactor does NOT fire (because the event parks). The test is
updated to assert the new behavior.

## Idempotency contract (pre-existing, made explicit)

Projections must be idempotent. This was always implicit because
`queue.enqueue` could fail after projections succeeded, causing them to
re-run on the next loop iteration. The existing path was rare; the new path
(any projection failure → retry) makes it common. The contract isn't new
but the operational pressure on it is. Document loudly.

## Code changes

### `modules/causal/src/types.rs`

- Remove `pub struct ProjectionFailure`.
- Remove `pub projection_failures: Vec<ProjectionFailure>` from `IntentCommit`.
- Update `IntentCommit::park` and `IntentCommit::skip` constructors.

### `modules/causal/src/job_executor.rs`

- Replace the projection loop body. Run all projections, collect `Err`s into
  a local `Vec<(String, String)>` of `(projection_id, error)`. After the loop,
  if non-empty, return `Err(anyhow!("..."))` with details.
- Stop importing `ProjectionFailure`.

### `modules/causal/src/memory_store.rs`

- Delete the `for failure in commit.projection_failures { eprintln!(...) }`
  block.

### `modules/causal/src/lib.rs`

- Remove `ProjectionFailure` from public re-exports.

### `modules/causal/src/reactor/types.rs`

- Add doc-comment on `Projection<D>` making the idempotency contract loud,
  noting failure semantics.

### Tests

**Update:**
- `engine_integration.rs::projection_error_does_not_stop_other_handlers`:
  invert. Assert reactor never fires; event eventually parks.
- `engine_integration.rs::projection_error_recorded_as_projection_failure`:
  invert. Assert the second projection runs on retry attempts (because the
  first projection re-runs each retry too), but cursor never advances past
  the failing event.

**Add:**
- New test: `projection_failure_does_not_advance_cursor`. Emit event,
  projection always fails. Assert checkpoint is unchanged after settle returns
  (settles on park, not success).
- New test: `projection_recovery_succeeds_after_transient_failure`. Projection
  fails N times then succeeds. Assert cursor advances and reactors fire.

### Version + CHANGELOG

- Bump `modules/causal/Cargo.toml`: `version = "0.2.0"`.
- Update workspace `Cargo.lock` accordingly (cargo handles).
- Add CHANGELOG entry under `## [0.2.0]` with:
  - **Breaking**: projection failure no longer silently advances the
    dispatch cursor; event retries and parks instead.
  - Note: previously-silent projection failures will become visible as
    parked events on upgrade. Consumers should monitor parked-event count.
  - Note: projections must be idempotent (was always required, now
    operationally enforced).

## What this fix doesn't do

Three known limitations, all deferred:

1. **In-memory retry counter.** `engine.rs:371`'s `event_attempts: HashMap`
   is in-process memory. Restart resets the counter. A persistent projection
   bug under CrashLoopBackoff retries forever, never parks, blocks all
   downstream events. Not worse than today (today never parks at all from
   this path), but worth fixing. Follow-up issue.

2. **No backoff between event retries.** `engine.rs:485-489` `break`s and
   the outer loop retries immediately. A 200ms transient blip burns the retry
   budget in milliseconds. Park reason becomes "transient" instead of
   recoverable failure. Follow-up issue.

3. **No per-event-per-projection success ledger.** Two projections, A
   succeeds, B fails: on retry, A re-runs (idempotent: must be), B re-runs.
   Wasteful for expensive projections; a correctness footgun for any consumer
   with a non-idempotent projection that slipped past the contract. The right
   primitive is a `(event_id, projection_id) → succeeded` ledger that lets
   the engine skip already-succeeded projections on retry. This also overlaps
   with the async-projection design (see followup plan). Follow-up issue.

These three are filed as follow-up issues. None block this fix.

## Migration

Consumers upgrading from 0.1.x to 0.2.0:

1. **Audit projections for idempotency.** Any projection that does INSERT
   without ON CONFLICT, increments a counter, or sends a notification must be
   fixed before upgrading. (Notifications belong in reactors with `ctx.run()`,
   not projections.)

2. **Watch the parked-event count after deploy.** Previously-silent failures
   surface as parks. Could be an avalanche on systems that have been quietly
   broken.

3. **No code changes for compliant consumers.** Sync projections behave the
   same on the success path. Failure path now blocks dispatch instead of
   silently dropping.

The follow-up async-projection plan (`2026-05-04-feat-async-projections-plan.md`)
introduces an opt-in `Async` mode for projections that should NOT block
dispatch. That plan is deferred; this PR is sync-only.

## Definition of done

- `ProjectionFailure` and `IntentCommit.projection_failures` removed from
  the codebase. `grep -r ProjectionFailure` returns only doc references.
- `process_event_inner` returns `Err` on any projection failure.
- `MemoryStore::enqueue` no longer has a projection-failure handling block.
- All existing tests pass except the two that locked in the bug; those are
  updated to reflect the new behavior.
- New test confirms cursor doesn't advance on projection failure.
- New test confirms recovery after transient failure.
- `Projection` rustdoc states the idempotency contract loudly.
- Crate version bumped to `0.2.0`.
- CHANGELOG updated.
- Three follow-up issues filed (or noted in CHANGELOG with intent to file).

## References

- `modules/causal/src/job_executor.rs:236-285` — current projection loop.
- `modules/causal/src/memory_store.rs:323-329` — the silent-advance block.
- `modules/causal/src/types.rs:123-130` — `ProjectionFailure` struct.
- `modules/causal/src/types.rs:140-194` — `IntentCommit` and constructors.
- `modules/causal/src/engine.rs:438-453` — event-retry counter (path that
  takes over after this fix).
- `modules/causal/tests/engine_integration.rs:2603, 2635` — the two tests
  that lock in the buggy behavior.
- `docs/plans/2026-05-04-feat-async-projections-plan.md` — followup design
  for the async-projection mode that lets consumers opt out of "failure
  blocks dispatch."
