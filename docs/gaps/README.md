# Known gaps — recoverability & event-sourcing fail-safes

Outstanding findings from the 2026-06-30 recoverability hazard hunt
(full audit: `docs/plans/2026-06-30-hazard-hunt-recoverability-audit.md`).
Each file is self-contained: finding, severity, evidence (file:line), a
RED-test sketch, and the recommended/decided fix. Pick up independently.

**Already shipped (not gaps — for context):** H3 (PITR cursor clamp in
`build()`), H4 (settle liveness ceiling), H5 (monotonic checkpoint `advance`
split from absolute `set`), H9 (in-memory effect-store build guard — was
already present). See branch `hardening/recoverability-hazards`.

**⚠ MERGE GATE:** before merging this branch to `main`, clear
[the live-Postgres validation](2026-06-30-merge-gate-live-postgres-validation.md)
— H5's atomic `GREATEST` advance was authored/reviewed but not executed against a
real database (none available at authoring time).

**What is solid (verified, no gap):** Postgres catch-up ordering (advisory-lock
mitigation), OCC, exactly-once reactor emission, divergent-redelivery handling,
dual-write atomicity (log is sole truth), checkpoint↔state consistency,
boot-cancel race, no-truncation invariant.

| Gap | Title | Severity | Class | Decided direction |
|---|---|---|---|---|
| [H1](2026-06-30-h1-projector-poison-wedge.md) | Projector poison wedge | CRITICAL | Correctness | Mirror reactor taxonomy (park + advance) |
| [H2](2026-06-30-h2-event-upcaster-seam.md) | No event versioning / upcasting | HIGH | Correctness | Build the upcaster seam |
| [H6](2026-06-30-h6-reactor-multi-output-non-atomic.md) | Reactor multi-output non-atomic | MEDIUM (gated) | Correctness | Document contract; full fix needs backend per-event positions |
| [H7](2026-06-30-h7-causation-depth-ceiling.md) | No causal-cycle failsafe | MEDIUM | Availability/safety | Causation-depth ceiling |
| [H8](2026-06-30-h8-snapshot-fold-version.md) | Stale-but-valid snapshot | MEDIUM | Correctness | Snapshot fold-version tag |
| [H10](2026-06-30-h10-pg-lock-regression-test.md) | PG ordering-lock regression guard | LOW | Test-only | Deterministic two-connection PG test |
