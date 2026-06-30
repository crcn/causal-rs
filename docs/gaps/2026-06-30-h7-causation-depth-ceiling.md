# H7 — No causal-cycle / causation-depth failsafe (MEDIUM, availability/safety)

**Status:** open. **Decided direction:** causation-depth ceiling.

## Finding
A reactor whose output kind matches its own trigger kind (or a multi-hop cycle
through other reactors) loops **unbounded**. Identity-keyed dedup does **not**
stop it: each generation's trigger has a different `event_id`, so each derived
output has a different `event_id` — no dedup-hit. `MAX_PENDING` (4096,
`reactor_runner.rs:73`) only *paces* ingestion; it does not break the cycle. The
causal tree and the log grow without bound until storage backpressure. There is
no depth counter and no self-trigger lint (grep: no `max_depth` /
`causation_depth` / `recursion_limit` in `modules/causal/src`).

## Evidence
- `reactor_runner.rs:73` (`MAX_PENDING` — paces, doesn't bound total emissions).
- Output `event_id` derivation `reactor_runner.rs:123-144` (fresh per generation).
- No depth/cycle guard anywhere (grep-confirmed).

## Reproduction / RED test
Register a reactor whose `Trigger` and emitted output share a kind (direct
self-trigger). Drive N dispatch cycles. Assert the log grows without bound today
(no ceiling stops it). GREEN target: emission is rejected/parked once the
causation depth exceeds a configurable limit.

## Recommended fix (decided: depth ceiling)
Carry a **causation depth** counter in event metadata (incremented from the
trigger to each output). When an output would exceed a configurable ceiling,
**park/error** instead of emitting, with a clear diagnostic naming the cycle.
A runtime failsafe catches all cycles, including multi-hop, which a static
self-trigger lint alone would miss. Open design points: default ceiling value,
where the counter lives (metadata key), interaction with legitimate deep chains,
and the park vs hard-error choice.
