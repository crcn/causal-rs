---
title: "feat: Async projections with per-projection cursors"
type: feat
date: 2026-05-04
status: rejected
rejected_in_favor_of: docs/brainstorms/2026-03-09-replay-projection-library.md
---

# Async projections with per-projection cursors — REJECTED

## Status

**REJECTED.** The library already solved this, with a different architecture
than what this plan proposed. See "Why rejected" below.

This document is preserved as a record of the pressure-test, not as a roadmap.

## What this plan originally proposed

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
