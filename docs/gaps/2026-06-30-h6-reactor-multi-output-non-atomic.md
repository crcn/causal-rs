# H6 — Reactor multi-output emission is non-atomic (MEDIUM, gated, correctness)

**Status:** open. **Decided direction:** document the contract + characterization
test now; the full atomic-batch fix is deferred (needs a backend trait change —
see below).

## Finding
A reactor emitting N outputs appends each via a **separate** `append_to_stream`
in a loop (`reactor_runner.rs:1171-1294`), not one atomic batch. If `react()`
emits a different output *set* across a retry — e.g. `[A, B]` on attempt 1 (both
appended, crash before ack) then `[A]` on attempt 2 — output **B is orphaned**
in the log: it persisted, the reactor no longer emits it, and **no divergence
fires** (B's identity-keyed `event_id` is never re-derived, so there is no
dedup-hit to compare).

**Gated:** this requires the reactor to be nondeterministic in *which* outputs
it emits — a determinism violation the framework already disclaims. Payload-level
nondeterminism (same output, different bytes) *is* caught by divergence
detection. And B is a **valid, durably-committed event** that downstream already
consumed — not corruption, just an event the reactor's current code no longer
"intends."

## Why the full fix is deferred (the real cost)
The clean fix is to batch same-stream outputs into one atomic
`append_to_stream(subject, id, Any, vec![...])` (the backend already has a
torn-batch guard, `memory_store.rs:521-533`). **But** the per-output tail of the
emission loop needs each output's individual committed position twice — the
workflow high-water bump (`reactor_runner.rs:1261`) and the engine-registry fold
(`:1277`). `WriteResult` returns a **single** `position`/`revision`
(`types.rs:155-161`), so a batch append loses the per-output positions. Fixing
properly requires either:
- **(a)** changing `EventLogBackend` so a batch append returns **per-event**
  `WriteResult`s — ripples through the trait, MemoryStore, PG, Kurrent,
  conformance, and every caller (large, high-risk to the core append primitive); or
- **(b)** deriving each position arithmetically from the batch's final position
  — **unsafe**: Postgres does not guarantee the N rows of one multi-row INSERT
  get *contiguous* `BIGSERIAL` values (a concurrent `nextval` can interleave).

Option (a) is the only correct path, and it is disproportionate to a gated,
low-severity, arguably-not-even-wrong gap.

## Evidence
- `reactor_runner.rs:1171-1294` (per-output append loop).
- `reactor_runner.rs:1261` (per-output hw bump at `write.position`), `:1269-1293` (per-output fold).
- `types.rs:155-161` (`WriteResult` is single-position).
- `memory_store.rs:521-533` (existing torn-batch guard, supports atomic multi-event append).

## Recommended action now (cheap, safe)
1. **Contract:** make "a reactor's output *set* must be deterministic across
   retries" an explicit line in the `Reactor::react` doc — the framework already
   relies on this; payload determinism is enforced, set-determinism is not.
2. **Characterization test:** pin the current behavior (a changed output set
   leaves an orphaned, non-retracted, non-divergent output) so a future change
   is noticed.

## If revisited: do the backend change
Bundle option (a) with any other work that already touches the `EventLogBackend`
return shape. Then batch per-stream and map per-event `WriteResult`s to the
hw-bump and fold tail.
