# Hazard hunt: recoverability & event-sourcing fail-safes (causal 0.16.0)

**Status:** original hunt record. Since authoring, **H3/H4/H5 have shipped** and **H9 was found
already-fixed** (branch `hardening/recoverability-hazards`); H1/H2/H6/H7/H8/H10 are tracked as
living gaps in `docs/gaps/`. The per-finding prose below is the as-discovered record; the
**status matrix at the end** and `docs/gaps/README.md` reflect current state. Findings are from
five parallel adversarial hunts, each spot-checked against source. Goal: survive a "nuclear"
situation (crash, PITR restore, schema drift, poison data, failover) with **correctness** as the
non-negotiable.

**Headline:** the *core event-sourcing engine is genuinely robust* — Postgres ordering, OCC,
exactly-once reactor emission, dual-write atomicity, and checkpoint↔state consistency are all
sound and, in several cases, defended by name with passing stress tests. **The exposure is in
lifecycle / recovery / ops wiring**: heals that ship but aren't armed, a poison-handling
mechanism that exists for reactors but was never extended to projectors, and the absence of an
event-versioning layer. Two findings are correctness-critical; the rest are recovery/liveness.

---

## What is SOLID (verified, no action)

| Area | Why it holds | Evidence |
|---|---|---|
| **Postgres catch-up ordering** (the #1 black swan) | Global `pg_advisory_xact_lock(0xCA05,0xA1)` taken by both & only the two `causal_log` writers before position assignment, held to commit → position order == commit order. The allocate-before-commit skip is structurally impossible. | `event_log.rs:25-51,212`, `event_projector.rs:92`; stress test `conformance::concurrent_appends_are_tailable_without_loss` |
| **OCC / append** | Typed `ConflictError`, reload+re-decide, bounded `MAX_OCC_RETRIES=16` w/ jittered backoff; atomic check-and-write under mutex. No lost update, no livelock. | `engine.rs:1743-1901`; conformance `append_to_stream_rejects_stale_expected`, `expected_revision_ahead_of_head_is_rejected` |
| **Exactly-once reactor emission** | Output `event_id = v5(consumer∥trigger∥kind∥subject∥nth)`; log dedups on it. Emission idempotency does NOT depend on a durable checkpoint. | `reactor_runner.rs:123-144`; `memory_store.rs:454-518`, `event_log.rs:134-168` |
| **Divergent redelivery** | Nondeterministic re-decision under same `event_id` is detected; original row authoritative, accepted + shouted, never overwritten, never retried-forever. | `memory_store.rs:471-512`, `reactor_runner.rs:1211-1253` |
| **Dual-write atomicity** | Log append is sole truth; per-stream batch is one atomic append; reactor output is itself the log entry (no outbox needed). | `engine.rs:1743-1901,1755`; `memory_store.rs:437-589` |
| **Checkpoint↔state consistency** | Aggregate state never persisted with checkpoint; recomputed from log on restart; cursor advances only post-fold; `set_state` monotonic. | `projection_runner.rs:244`, `aggregator.rs:718-738` |
| **Boot-cancel race (0.15.2)** | Standalone `append_workflow_cancelled` + fence rebuilt before any consumer spawns; deterministic idempotent marker. | `engine.rs:463-490,1364-1384` |
| **No truncation** | No delete/scavenge/`$tb`/retention anywhere; streams dense from 0; conformance forbids sparse writes. | grep-confirmed; `conformance.rs` ordering tests |

---

## FINDINGS (prioritized)

### H1 — CRITICAL (correctness). Projector / multi-projector poison wedge; no park/skip.
A poison event (payload that no longer deserializes into the registered type, or a `project()`
that deterministically errors) makes `ProjectionRunner::step` / `MultiProjectorRunner::step`
return `Err` **before advancing the checkpoint**. The supervisor (`engine.rs:2778-2825`) retries
a *deterministic* failure **forever, no ceiling, cursor never moves** → the projection is wedged
permanently, and **replay-from-zero re-poisons on the same event**. This is by explicit design:
the runner header says failure handling is "`BlockUntilFixed` only … `AdvanceAfter` (park-and-skip)
lands in a later phase" (`projection_runner.rs:11-14`) — and `AdvanceAfter` is **not implemented**
(only those comment lines mention it). Reactors handle this correctly (they PARK poison and
advance the ack-floor — `reactor_runner.rs:1028-1040,931-951`), so the right shape is already in
the codebase; it was simply never extended to projectors.
- **Recovery today:** none automated. Manual: fix payload upstream, ship an upcasting
  deserializer, or DB-surgery the checkpoint. `settle`'s wedge guard *detects* it (returns Err
  after N failures) but does not *recover*.
- **Fix (DESIGN DECISION):** add a projector failure policy mirroring the reactor taxonomy —
  classify poison vs transient, and on poison either **park** (emit a terminal/dead-letter fact +
  advance) or **skip-and-record**. This is the "radically different approach" worth discussing:
  what is the desired projector semantics on poison — park, skip, or operator-gated quarantine?
- **RED test:** register an aggregator/projector for `EventV2{a,b}`; append a historical
  `{"a":1}` payload; `step()` → asserts it currently returns Err and the checkpoint is frozen
  (wedge). Desired-state test asserts park/skip + advance once the policy exists.

### H2 — HIGH (correctness, structural enabler). No event versioning / upcasting.
Events are immutable and live forever, but there is **no upcasting, version tag, or migration
layer**. `_schema_v` is a user-set metadata key the engine never reads. Consequences:
- **Field removed/renamed/retyped within a still-registered type** → `serde_json::from_value`
  fails → poison (feeds H1 for projectors; parks for reactors but the old event then never folds
  successfully anywhere).
- **Event-type rename** → old `event_type` matches no aggregator → **silent identity skip**
  (`apply_event` matching filter empty; projector `checkpoint.set + continue`). Silent
  correctness loss, distinct from the wedge.
- **Fix (DESIGN DECISION):** introduce an event-upcasting seam (per-type version + registered
  upcaster `oldJSON → currentJSON` applied at read/fold time), the standard KurrentDB approach.
  Big, but it's the root enabler for H1's "old event folds successfully" and for safe schema
  evolution. Discuss scope/timing.

### H3 — MEDIUM-HIGH (correctness/recovery). `clamp_ahead_of` ships but is never armed → silent
event skip after a point-in-time restore.
After a PITR/snapshot restore the log tip is rewound to an earlier position, leaving a consumer's
durable cursor **past** the new tip. On restart the consumer reads `read_all(staleCursor)` →
empty → **silently skips every event between the real tip and the stale cursor**, including new
events appended after restore. `clamp_ahead_of(tip)` exists precisely to fix this (clamp cursors
down to tip, never to 0, avoiding a divergence storm) — but it has **zero production callers**
(verified: only the trait default, two backend impls, one unit test). `build()` already reads
`latest_position()` during seeding (`engine.rs:1392`), so the heal is one call away.
- **Fix (BOUNDED):** in `build()`, call `clamp_ahead_of(latest_position())` for each consumer
  cursor (or behind a `RunnerConfig` flag), and at minimum emit a loud diagnostic when any stored
  cursor > tip. Low risk, high recovery value.
- **RED test (MemoryStore):** append 3 events; set a consumer checkpoint to position 99
  (simulating a stale post-restore cursor); append 2 more (positions 4,5); step the runner →
  assert it currently never delivers 4,5 (silent skip). GREEN: build/runner clamps to tip → 4,5
  delivered.

### H4 — MEDIUM (liveness/recovery). `settle()` hangs forever on an *absent* (not-failing,
not-running) consumer.
The wedge guard fires only on *counted failures* (`SETTLE_WEDGE_FAILURES`, no wall-clock —
`engine.rs:2335-2360`). A consumer whose supervisor task panicked at the framework level and
wasn't respawned, was never spawned, or is blocked on a dead peer's lease increments no failure
counter → `wedged()` returns `None` → `settle` polls `drained` indefinitely. Tests wrap settle in
`tokio::time::timeout`, implying production callers must too — there is no built-in liveness ceiling.
- **Fix (BOUNDED):** optional wall-clock deadline / no-global-progress ceiling on `settle` (a
  liveness failsafe distinct from the per-consumer failure guard). Surfaces a typed timeout error.
- **RED test:** engine with one projector; emit so `hw` advances; never spawn/step that
  projector's supervisor (simulate a silently-dead task); `settle` under a test timeout → asserts
  it hangs today (should return a typed liveness error within a bound).

### H5 — MEDIUM (correctness, config-dependent). PG checkpoint `set` is an unconditional upsert
(no CAS) → backwards regression on misconfigured multi-node.
`reactor_checkpoint.rs:60-76` does `ON CONFLICT DO UPDATE SET position = EXCLUDED.position`. Safe
*only* because the consumer is the single writer — which rests entirely on the `ConsumerLeasor`,
which is **opt-in** (`engine.rs:889` defaults `None`; "Without a leasor, the engine assumes
single-engine"). A two-node deploy that forgets `with_consumer_leasor` gets two live workers; a
lagging worker's `set` overwrites a more-advanced cursor **backwards** → reprocessing + (for
nondeterministic reactors) a divergence storm.
- **Fix (BOUNDED, defense-in-depth):** make the PG `set` monotonic —
  `SET position = GREATEST(causal_checkpoints.position, EXCLUDED.position)` — so checkpoint
  correctness no longer depends on remembering the lease. (Note: a *legitimate* clamp-down for H3
  must then use an explicit clamp path, not `set`.)
- **RED test:** two reactor runners, same `consumer_id`, same PG checkpoint store, no leasor;
  A advances to 100, B (lagging) sets 40 → assert stored position stays ≥100 (fails today).

### H6 — MEDIUM (correctness, gated). Reactor multi-output emission is N separate appends, not one
atomic batch.
A reactor emitting `[A,B,…]` appends each output via a separate `append_to_stream`
(`reactor_runner.rs:1171-1208`). A crash after k of N (before ack) replays `react()`, re-deriving
`0..k` (deduped) + `k..N`. Correct **iff** the reactor deterministically re-produces the same
output *set* and order. A reactor that emits `[A,B]` then `[A]` orphans B in the log with no
divergence signal (B's `event_id` is never re-derived). Gated behind reactor output-*set*
nondeterminism the framework already disclaims, but the fix is cheap.
- **Fix (BOUNDED):** append the whole same-stream output run as **one atomic batch**
  (`append_to_stream` already supports multi-event batches with a torn-batch guard,
  `memory_store.rs:521-533`). Closes the partial-emit window outright.
- **RED test:** reactor emits `[A,B]` then `[A]`; fail the ack after both appends on attempt 1;
  restart; assert B is orphaned + no divergence reported today. GREEN: atomic batch makes
  partial-emit impossible.

### H7 — MEDIUM (availability/safety). No causal-cycle / causation-depth failsafe → unbounded log
growth.
A reactor whose output kind matches its own trigger kind (or a multi-hop cycle) loops **unbounded**
— identity dedup does NOT stop it (each generation has a fresh `event_id`). `MAX_PENDING` (4096)
only *paces* it; the log fills without limit. No depth counter, no self-trigger lint
(verified: no `max_depth`/`causation_depth`/`recursion_limit` in the source).
- **Fix (DESIGN DECISION):** a causation-depth counter in event metadata with a configurable
  ceiling (park/error past it), and/or a build-time lint rejecting a reactor whose output kinds
  intersect its trigger kinds. Discuss which.
- **RED test:** self-triggering reactor; run N dispatch cycles; assert the log grows without
  bound today (no ceiling stops it).

### H8 — MEDIUM (correctness). Stale-but-deserializable snapshot → silently wrong state on
fold-logic change.
The `Snapshot` struct (`types.rs:268-274`) has no schema/fold-version tag; `snapshot_at_version`
tracks stream position, not fold-*code* version. A snapshot produced by an older `Apply` impl
deserializes fine and is trusted verbatim (only newer events replay on top) → silently wrong
state, no error, no self-heal (self-heal triggers only on *deserialize failure*).
- **Fix (BOUNDED-ish):** add a `fold_version`/schema tag to `Snapshot`; on load, if the tag
  mismatches the registered aggregator's current version, discard + rebuild from genesis. Needs a
  per-aggregate version the author bumps on fold-logic changes.
- **RED test:** snapshot under `Apply` v1; change `Apply`; restore → assert `state_of` returns the
  stale v1 value with no error today.

### H9 — ALREADY FIXED (audit correction). Side-effecting reactor + default in-memory effect store.
**The dual-write hunt recommended a builder guard for this — but the guard already exists.**
`EngineBuilder::build()` hard-`bail!`s (`engine.rs:1327-1352`) when any reactor is registered and
neither `with_effect_store(<durable>)` nor `allow_in_memory_effect_store_for_tests()` was called
(the latter is what `EngineBuilder::memory()` sets). So a production engine with a reactor and the
in-memory default **cannot build**. The agent read the `contexts.rs:372` default but missed the
`build()` guard. **No action required.** (Verified by direct read of `build()`.)

### H10 — LOW (observability, defense-in-depth). PG ordering lock regression guard missing.
The advisory-lock mitigation for the Postgres ordering black swan (SOLID, above) is covered by a
*scheduling-jitter* stress test, not a *deterministic* two-connection adversarial test that would
fail if the lock were removed.
- **Fix:** add a raw two-connection SQL test (txn A gets position N, txn B gets N+1 & commits,
  tailer reads to N+1 & checkpoints, A commits → assert N never skipped). Proves the lock is
  load-bearing as a regression guard. Pure test addition; no production change.

---

## Severity / class / fix-type matrix

| # | Hazard | Severity | Class | Fix type | Status |
|---|---|---|---|---|---|
| H1 | Projector poison wedge | CRITICAL | Correctness | Design (DECIDED: mirror reactor taxonomy) | ⏳ Tier-2 |
| H2 | No event versioning/upcasting | HIGH | Correctness | Design (DECIDED: build upcaster seam) | ⏳ Tier-2 |
| H3 | `clamp_ahead_of` never armed → PITR skip | MED-HIGH | Correctness/recovery | Bounded | ✅ **shipped** (engine.rs build clamp; RED/GREEN verified) |
| H4 | `settle` hangs on absent consumer | MEDIUM | Liveness | Bounded | ✅ **shipped** (ConsumerHealth heartbeat + opt-in `with_settle_liveness_ceiling`; test green) |
| H5 | PG checkpoint `set` no CAS | MEDIUM | Correctness (config) | Bounded | ✅ **shipped** (monotonic `advance` split from absolute `set`; PG `GREATEST`; conformance test) |
| H6 | Reactor multi-output non-atomic | MEDIUM | Correctness (gated) | Bounded (delicate) | ⏳ next — delicate refactor of the divergence-handling emission loop |
| H7 | No causal-cycle failsafe | MEDIUM | Availability/safety | Design (DECIDED: depth ceiling) | ⏳ Tier-2 |
| H8 | Stale-but-valid snapshot | MEDIUM | Correctness | Bounded-ish | ⏳ Tier-2 |
| H9 | In-memory effect-store trap | LOW-MED | Correctness (config) | — | ✅ **already fixed** (build() hard-bails, engine.rs:1327) |
| H10 | PG-lock regression guard | LOW | Observability | Test-only | ⏳ pending |

---

## Recommended sequencing

**Tier 1 — bounded fixes, do now (RED test → fix), low risk, high recovery value:**
H3 (arm `clamp_ahead_of` in `build()` + diagnostic), H4 (settle liveness ceiling), H5
(`GREATEST` checkpoint set), H6 (atomic multi-output append), H9 (effect-store lint), H10
(PG-lock regression test).

**Tier 2 — design decisions, discuss desired semantics first, then RED test → implement:**
H1 (projector poison policy: park vs skip vs quarantine), H2 (event upcasting seam — the root
enabler; scope/timing), H7 (cycle failsafe: depth ceiling vs lint vs both), H8 (snapshot
fold-version tag — needs an author-bumped per-aggregate version convention).

The Tier-1 set is independently shippable and materially improves "nuclear" recoverability
without changing core semantics. Tier-2 is where "radically different approaches" may be
warranted and correctness trade-offs must be chosen deliberately.
