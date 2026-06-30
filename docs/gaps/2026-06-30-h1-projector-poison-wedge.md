# H1 — Projector / multi-projector poison wedge (CRITICAL, correctness)

**Status:** open. **Decided direction:** mirror the reactor failure taxonomy
(poison parks + advances; transient retries; domain budget).

## Finding
A poison event — a payload that no longer deserializes into the registered
type, or a `project()` that deterministically errors — makes
`ProjectionRunner::step` / `MultiProjectorRunner::step` return `Err` **before
advancing the checkpoint**. The supervisor (`engine.rs` `supervise_one`,
~`:2778-2825`) retries a *deterministic* failure **forever, no ceiling, cursor
never moves** → the projection is wedged permanently, and **replay-from-zero
re-poisons on the same event**. By explicit design: the runner header says
failure handling is "`BlockUntilFixed` only … `AdvanceAfter` (park-and-skip)
lands in a later phase" (`projection_runner.rs:11-14`) — and `AdvanceAfter` is
**not implemented** (only those comment lines mention it).

Reactors handle this correctly: they classify poison and **park** it (emit a
terminal fact) while advancing the ack-floor (`reactor_runner.rs:1028-1040`,
`:931-951`; taxonomy in `failure.rs`). The right machinery already exists in
the codebase — it was never extended to projectors.

## Evidence
- `projection_runner.rs:11-14` (BlockUntilFixed-only design note; `AdvanceAfter` is a TODO).
- `projection_runner.rs:189-256` (fold/project `?` before checkpoint set).
- `multi_projector.rs:221-285` (same shape).
- `engine.rs:2778-2825` (infinite-retry supervisor; comment names the hazard).
- Contrast (correct): `reactor_runner.rs:1028-1040`, `:931-951`, `failure.rs`.

## Reproduction / RED test
Register a projector/aggregator for `EventV2 { a, b }` (b non-optional). Append
a historical `{"a":1}` payload for the same registered type. `runner.step(10)`:
- RED today: returns `Err`, `checkpoint.get(consumer)` unchanged (wedge).
- GREEN target: poison is parked (terminal fact emitted) + checkpoint advances;
  the projection stays live for subsequent events.
Replay-from-zero variant: recreate the consumer at cursor 0 → `ensure_hydrated`
errors on the same event every boot (no startup path skips it).

## Recommended fix (decided: mirror reactor taxonomy)
Give the projector runners a failure policy mirroring `failure.rs`:
- classify poison (deserialize failure, structural) vs transient vs domain;
- on poison: emit a terminal/dead-letter fact (built-in `causal:projection_failed`
  on the event's subject history, paralleling `causal:reaction_failed`) and
  **advance** past it;
- transient: bounded retry; domain: `max_attempts` then park.
Share the taxonomy with reactors rather than duplicating it.

## Notes
Strongly coupled to **H2**: parking contains the blast radius (one event), but
only an upcaster lets an evolved-but-valid old event *fold successfully* instead
of being parked. Sequence H2 first or together.
