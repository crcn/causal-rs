# Decision Records — design spec (target: 0.19.0)

Companion to rootsignal's `docs/audits/2026-07-02-causal-adversarial-audit.md`.

## The problem this kills

Redelivery **re-decides**: the reactor body re-executes and the runtime hopes
recomputation is byte-identical. The determinism contract is unenforceable in
Rust; divergence detection only catches the *collision* case (same event_id,
different payload). The *disjoint* case is silent:

```text
Execution A (crash mid-output-loop): body computes outputs for subjects {X, Y}
  → Out(X), Out(Y) appended, crash before checkpoint
Execution B (redelivery): nondeterministic body computes {X, Z}
  → Out(X) dedup-hits; Out(Z) has a NEW event_id → appends as a fresh fact
  → Y stays in the log
Log = {X, Y, Z} — a chimera: a decision no execution made. No divergence
fires. Nothing is logged. (reactor_runner.rs:1289-1431 + derive_output_event_id)
```

Every layer of divergence machinery (detection, `first_diff_path`,
accept-and-advance, the divergences table, `reactor_divergence` observer hook,
downstream `serde_sorted` patches) is scar tissue around this one choice.

## The fix: decide once, durably; redelivery replays the record

```text
first delivery of trigger T to consumer C:
  fence/depth checks (unchanged)
  outputs = react(T, ctx)                  // effects memoize across RETRIES, as today
  sealed  = decisions.seal(C, T.event_id, outputs)   // FIRST-WRITE-WINS, atomic
  append each sealed.outputs[i] to its subject stream  // idempotent, from SEALED
  checkpoint advance (unchanged)

redelivery of T:
  if let Some(rec) = decisions.get(C, T.event_id):
      append any missing rec.outputs (idempotent completion) — BODY NEVER RUNS
  else: first-delivery path (crash happened before seal — a decision was
        never made, so re-deciding is correct, not a hazard)
```

Two invariants carry everything:

1. **A trigger's outputs enter the log only from a sealed record.** Even the
   sealing execution appends from `sealed` (the store's canonical row), not
   from its local `outputs` — so two racing executions (deploy overlap, lease
   handoff) both append the SAME batch regardless of which one sealed.
2. **Seal is atomic and first-write-wins.** One write, one row. There is
   exactly one decision per (consumer, trigger) ever.

What this buys, structurally:

- **Chimera: impossible.** Partial appends complete *from the record*.
- **H6 (non-atomic multi-output) closes without touching backend append
  traits.** The record IS the atomicity; the per-output append loop becomes an
  idempotent, resumable projection of it. No Kurrent cross-stream atomic
  append needed.
- **Deploy-overlap double-processing (audit Swan #5) defanged** by invariant 1.
- **Reactor determinism demotes from correctness contract to hygiene.**
  Nondeterministic bodies waste retry cost at worst; they can no longer
  corrupt the log. The contract stays load-bearing ONLY for folds/aggregates
  (state reconstruction), where it is actually enforceable (pure `apply`).
- **Zero-output reactions seal an empty record** — kills the "processed vs
  never ran" ambiguity with no log tombstone.

## Trait + backends

```rust
/// One durable decision per (consumer, trigger). First write wins.
#[async_trait]
pub trait DecisionStore: Send + Sync {
    /// Insert-if-absent, then return the canonical row (ours or the racer's).
    async fn seal(&self, rec: DecisionRecord) -> Result<DecisionRecord>;
    async fn get(&self, consumer: &str, trigger_event_id: Uuid)
        -> Result<Option<DecisionRecord>>;
    /// GC — floor-based, same lifecycle as effect GC (reactor_runner.rs:1696-1730).
    async fn remove(&self, consumer: &str, trigger_event_id: Uuid) -> Result<()>;
}

pub struct DecisionRecord {
    pub consumer: String,
    pub trigger_event_id: Uuid,
    pub outputs: Vec<EventData>,   // full envelopes: event_id, kind, subject_id,
                                   // category, payload, metadata (incl. depth),
                                   // workflow_id, causation_id
    pub sealed_at: DateTime<Utc>,
}
```

This is the effect store generalized from per-label values to the whole output
batch — reuse its shape. Postgres backend mirrors `PgEffectStore`'s
first-write-wins CTE (`INSERT ... ON CONFLICT DO NOTHING` + `SELECT`):

```sql
CREATE TABLE causal_decisions (
  consumer          TEXT        NOT NULL,
  trigger_event_id  UUID        NOT NULL,
  outputs           JSONB       NOT NULL,
  sealed_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (consumer, trigger_event_id)
);
```

`MemoryStore` implements it natively (test path). Ship the DDL via
`ensure_schema` + exported `DECISION_SCHEMA_SQL` from day one — do not repeat
the divergences-table footgun (0.18.0 changelog).

## Runner changes (`reactor_runner.rs`)

- `process_trigger`: before dispatching to the body, `decisions.get(...)` —
  hit ⇒ completion path (append missing outputs from record → ack → floor);
  miss ⇒ existing attempt path, with `seal` inserted between body success and
  the append loop, and the loop iterating `sealed.outputs`.
- **event_id derivation moves to seal time** and is stored in the record.
  `derive_output_event_id` (`reactor_runner.rs:129-150`) stays as-is — it now
  runs once per decision instead of once per execution, which removes the
  `nth`-reset-between-attempts class of bug entirely.
- **Park path unchanged.** A body failure means no record exists; retries and
  terminal park work as today. (Independent 0.19 fix from the audit: swap
  `clear_reactor_attempts` before the observer hook, `reactor_runner.rs:1521-1640`.)
- **Fence + H7 depth ceiling unchanged**, checked before seal. A sealed record
  for a workflow cancelled *after* sealing still appends — the decision
  happened pre-cancel; that is correct, not a leak.
- **GC**: extend the existing floor-based sweep to also `remove` decision
  records. Optimization (later): effects for a trigger can GC at seal time,
  since the body never runs again.

## What gets deleted

- The divergence **handling** path in the runner (accept-and-advance,
  `reactor_divergence` observer traffic, `first_diff_path` diagnostics on the
  redelivery path). Appends from a sealed record are byte-identical by
  construction.
- **Keep** `DivergentRedelivery` in the backends, repurposed as an integrity
  assert: post-records, its firing means record corruption or a genuine bug —
  it should now be LOUD (error, park), not accepted.
- The divergences table stops receiving rows; mark deprecated, drop in 0.20.
- The double-run determinism-verification feature (specced this session, never
  built): dead — it guards a contract that no longer bears correctness load.

## Builder surface (breaking, honest)

```rust
EngineBuilder::with_decision_store(Arc<dyn DecisionStore>)   // required when
                                                             // reactors are registered
EngineBuilder::allow_in_memory_decision_store_for_tests()    // test escape hatch
```

Same gate shape as the 0.16 effect-store rule ("No Lying Defaults"): building
a reactor-bearing engine without a durable decision store is a loud error, not
a silent in-memory default. This makes 0.19.0 a breaking release; say so in
the changelog.

## Costs, named

- One extra durable write per reaction that emits (and one per zero-output
  reaction). Bounded by the same floor window as effects; fat downstream
  events (~1 MB `SourcesPrepared`) are double-stored only until GC.
- `get` lookup per delivery (one indexed PK read; cache-hot for live traffic).
- Migration: **additive** — in-flight triggers at upgrade have no record, take
  the first-delivery path, and their already-appended outputs dedup-hit
  exactly like today's recovery. One last recompute per in-flight trigger, then
  never again. No data migration.

## Tests (behavior names)

```text
sealed_decision_replays_without_reexecuting_the_body
crash_after_partial_append_completes_the_batch_from_the_record
nondeterministic_body_cannot_produce_a_chimera            // the audit's Swan #1, RED first
racing_executions_adopt_the_first_sealed_decision          // both append identical batches
zero_output_reaction_seals_an_empty_record_and_advances
crash_before_seal_re_decides_and_that_is_correct
body_failure_retries_then_parks_without_sealing
gc_removes_records_behind_the_floor_and_never_live_ones
divergent_redelivery_after_records_is_a_loud_integrity_error
engine_with_reactors_and_no_decision_store_refuses_to_build
```

## Sequencing (0.19.0)

1. `DecisionStore` trait + Memory/PG backends + conformance suite entries.
2. Runner integration (seal / replay / GC) + the test list above.
3. Delete divergence handling; repurpose the backend assert.
4. Park-order swap (independent, same release).
5. Changelog: BREAKING (builder gate), with the chimera writeup as rationale.

Settle-ceiling default, checkpoint debounce, `PgEventProjector` park parity,
H8 fold-version, H2 upcasters: separate audit items, not part of this spec.

---

# Amendments after adversarial attack (2026-07-02)

A hostile spec review + live-Kurrent probing (see rootsignal
`docs/audits/2026-07-02-causal-adversarial-audit.md`, v2) confirmed the core
(racer adoption, H7 depth preservation, cancel containment, seal-time
derivation, divergence-machinery deletion safety) and broke five things.
These amendments OVERRIDE the corresponding sections above.

## A1. GC is retention-based, not floor-based (overrides "GC — floor-based")

Floor-based `remove()` defeats invariant 2: `PgConsumerLeasor` is a session
advisory lock with **no fencing token** — a zombie holder can re-seal after
the first decision is GC'd and resurrect the chimera. GC by AGE (default
retention measured in days, config), with the floor as a *minimum* bound —
never remove a record the floor hasn't passed, and never remove one younger
than the retention window. Cheap: records are small except where payloads are
fat, and fat payloads age out too.

## A2. Blocked on Kurrent global event_id dedup (new prerequisite)

The completion path assumes re-appending from the record is idempotent.
**On Kurrent it is not**: `append_any_idempotent` dedups only within a
`max(4·batch, 64)`-event tail window (`kurrent_event_log.rs:130`); beyond it
the re-append lands a duplicate (empirically proven against live Kurrent).
Ship a global event_id index for the Kurrent backend (design freely: a
PG-side `causal_event_ids(event_id PK, stream, position)` registry consulted
on Any-appends is the obvious shape since PG is already a required backend) —
and while there: un-`#[ignore]` the PG/Kurrent conformance suites, add
>64-depth redelivery + divergence scenarios and C1c placement for Kurrent,
and remove `continue-on-error` from that CI job. Decision records without
this fix trade one corruption for another on the production backend.

## A3. Divergence handling is record-gated, not unconditionally loud
(overrides "repurposed as a loud integrity assert")

A third cause of divergence-fire exists post-records: redelivery of a trigger
whose record was **GC'd** (checkpoint regression — PG restore, operator
truncate, the zero-clamp bug). Unconditional loud-park would park-storm
terminal facts into run streams on every checkpoint regression — the exact
failure accept-and-advance was built to prevent. Rule: on divergence, consult
the decision store; **record present ⇒ loud (park, integrity error); record
absent ⇒ accept-and-advance with a warn** (legacy semantics — we re-decided
because we verifiably forgot).

## A4. Seal failure classification (new; the spec was silent)

Seal sits between body success and the append loop. A seal error is
**runner-infrastructure retry** (the same arm as append I/O errors — back off
and retry forever under the liveness ceiling), NEVER `classify()`-parked: a
routine PG blip must not mass-park succeeded work. Two sub-rules:
(a) sanitize `\u0000` escapes (and any other JSONB-rejected sequences) from
payload strings at seal time — scraped web content contains them, and an
unsanitized seal fails *deterministically*, which under infra-retry is a
permanent wedge; (b) the first-write-wins CTE must handle the
concurrent-insert zero-row case (retry the SELECT) — the PgEffectStore CTE it
copies can return zero rows under racing first-writes.

## A5. Completion path replicates the per-append side channels (new)

"Append missing outputs from record → ack → floor" omits two things today's
loop does per output: the settle high-water bump
(`reactor_runner.rs:1396-1398`) and the best-effort engine-registry fold
(`:1399-1429`). Replicate both on the completion path (dedup-hits return the
original coordinates, so the bump is safe), or `settle()` can return before
catch-up appends drain downstream consumers.

## A6. Empty records: measure before shipping (qualifies "zero-output
reactions seal an empty record")

Today a no-op reaction costs zero durable domain writes; the spec makes every
delivered trigger write+GC a row. In fan-out topologies where most deliveries
no-op (downstream registers 6 signal kinds × 4 consumer families), decision
traffic would dominate the causal-infra write path. Options, decide by
benchmark: (a) keep empty records (simplest, full processed-vs-never-ran
signal); (b) a compact decided-empty marker on the reactor-checkpoint row;
(c) empty-seal elision for consumers explicitly marked side-effect-free.
Do not silently ship (a) without the measurement.

## A7. Interplay with `skip_gap_on_start` (documentation requirement)

Consumers started at `StartPosition::Latest` skip their gap on boot: they
never redeliver — so they collect the record-replay benefit only for
in-process crash-redelivery and concurrent-instance overlap, while **silently
dropping gap triggers on every restart** (a data-loss trade downstream makes
~30 times, unaudited). The spec must document that decision records do NOT
protect skipped gaps, and the runner should log the skipped range loudly at
start so the trade is visible. Rename the API while touching it: the name
reads as an optimization; it means *drop data on restart*. Deprecate in favor
of `start_at_latest_dropping_gap` (old name kept one release as a deprecated
alias).

# DX hardening (ships with 0.19 unless noted)

The records surgery fixes correctness brittleness; these close the
developer-experience brittleness that remains. Rationale: the downstream
codebase had 5 reactors using `ctx.effect` correctly and 35 not — contracts
that don't teach or enforce themselves fail at real team sizes.

## D1. `EngineBuilder::in_memory_for_tests()` — ships WITH the decision gate

The gate adds a third mandatory store and a second test escape hatch; setup
is becoming a recital. One call that wires `MemoryStore` into every slot
(log, checkpoint, reactor checkpoint, snapshot, observer) and satisfies both
`allow_in_memory_*_for_tests` gates. Do not ship the new gate without this,
or every downstream test harness churns twice.

## D2. Sequencing guard: the completion path is blocked on Kurrent global dedup

Prose-ordering is brittle. The records PR must include a test that runs the
completion path against a stream where the original outputs are buried >64
events deep on the Kurrent backend — it FAILS (duplicates) until the global
event_id index lands, making the dependency executable instead of
documentary.

## D3. Typed settle timeout + per-reactor react-timeout override

The ceiling default creates a new caller-visible case whose meaning is
"still running", not "failed". Ship a typed error
(`SettleTimeout { workflow_id, last_progress }`) — never a stringy anyhow —
with docs stating explicitly that the work continues. The per-attempt `react`
timeout needs a per-reactor override (`.with_attempt_timeout(...)`):
downstream has legitimately-minutes-long LLM effects; a global default
without override is a new footgun.

## D4. Boot-time orphan detection (the rename trap)

Everything is keyed by consumer/kind strings; renames orphan checkpoints,
effects, and decision records with zero warning — the most likely brittleness
to fire in a fast-renaming pre-1.0 codebase. At build: warn loudly for any
checkpoint/decision/effect rows whose consumer id is not registered
("did you rename?"), and for event kinds present in the log with no
registered fold or trigger. Optional hard-fail flag for CI.

## D5. Fold determinism self-check (test engines only)

Records demote reactor determinism to hygiene; fold (`apply`) purity remains
fully load-bearing and has ZERO detection — a nondeterministic fold silently
produces different state on replay. In in-memory/test engines, fold each
hydrated stream twice and compare states; name the first differing subject.
(The double-run idea was wrong for reactors — the runtime no longer re-runs
them — but folds are pure functions where re-running IS the contract.)

## D6. Duplicate effect-label debug assert

`(consumer, trigger, label)` keying means a second `ctx.effect("fetch", ...)`
in one reaction silently replays the first's value. One debug-mode assert on
label reuse within a reaction kills the trap class.

## D7. Ship the lint config in-crate

The clippy `disallowed-methods` guardrail (`Uuid::new_v4`, `Utc::now` in
reactor modules) exists only as folklore. Publish it as a copy-pasteable
`clippy.toml` snippet in the repo (`lints/`) and reference it from the
reactor-authoring docs.

## D8. 0.20 design doc: Ctx-only I/O

The structural fix for `ctx.effect` being advisory: deps reachable ONLY
through an effect-scoped accessor (`ctx.io(label, |deps| ...)`), so
unmemoized I/O in a reactor body becomes unwritable rather than inadvisable.
Breaking API + real downstream migration — design doc in 0.20, not code in
0.19. Records make this non-urgent (the failure mode is cost/incoherence,
not corruption), but it is the only version where the next reactor cannot
reintroduce the pattern.

---

# Shipped in `feat/decision-protocol-side-doors` (2026-07-02)

A follow-up correctness audit found five "decision-protocol side doors" —
paths that produce or destroy a trigger's outcome WITHOUT going through the
seal→append→replay protocol. All five are fixed (TDD, one regression test
each). These amendments OVERRIDE the A3/A6 sections above where they conflict.

## S1. Fence rehydration fails loud, not open (#111)

`build()`'s cancel-fence rebuild used `if let Ok(markers) = read_stream(...)`,
so any storage error silently booted an EMPTY fence — and runners only learn
markers ABOVE their checkpoint from the live scan, so a marker below a cursor
was never re-learned and cancellation was resurrected for the process
lifetime. Now: bounded retry (a cold-pool blip is routine) then PROPAGATE —
refuse to boot with a fence that lies. An ABSENT control stream is still
`Ok(empty)` on every backend (verified), so fresh boots are unaffected. The
fence lock is no longer held across the read await.

## S2. Cancel fence-ack consults the decision store (#12)

Both fence gates acked a fenced trigger unconditionally, so a decision sealed
BEFORE the cancel — with its append loop crash-interrupted — was never
completed ("cancelled after sealing still appends" was violated; the log kept
a torn batch forever). Now, with a decision store, a fenced trigger flows to a
worker and hits the replay gate: a sealed record replays/completes its batch;
only a genuine get-miss (no decision was ever made) fence-acks without running
the body. `get()` errors retry as infra, never silently fence-ack a
maybe-incomplete batch. The no-store path keeps the legacy gate ack.
**Accepted residual:** gate fence-acks are still non-durable — a
fence-ack-then-peer-seals chimera survives, reachable ONLY via concurrent
same-consumer processes with a stale fence. Sealing a ∅ record on every
fence-ack was rejected (mass-cancel write amplification); S1 closes every
single-process leg.

## S3. Fence-consulted emptiness is not elidable (#113)

`.seal_empty_decisions(false)` let a body that early-exited empty because
`ctx.is_workflow_cancelled()` was true seal NOTHING; a redelivery where the
fence read false (S1's blip, or a lagging replica) re-decided a FULL batch.
Now `is_workflow_cancelled()` marks the reaction fence-consulted, and elision
is overridden for any fence-consulting body — the cancel outcome always seals.
**DX consequence (documented on the API):** elision and the recommended
`is_workflow_cancelled` early-exit are mutually exclusive; a fence-consulting
body seals every no-op decision regardless of the flag.

## S4. Park is a sealed decision (#3), and A3 becomes log-wins (#21)

**Park-as-decision.** A terminal park appended its fact directly and sealed
NOTHING; a crash before the ack re-ran the body, which could now SUCCEED and
seal a contradictory success — a chimera. Now a park SEALS a `parked`
`DecisionRecord` (new `parked: bool`) BEFORE appending the terminal fact, from
the sealed record, so redelivery replays it (body never re-runs). A silent
park (H7 cycle-guard, mapper-`None`) seals an EMPTY parked record. The DLQ
observer fires at-most-once — only on the execution that actually sealed the
park (`won && parked`), detected by byte-comparing the canonical record to the
one we sealed. A success execution that loses the seal race to a park record
adopts it and reports `parked=true` (so floor-GC keeps the trigger's effect
entries); a park that loses to a success reports `parked=false`.

**A3 re-scoped: log-wins reconciliation (OVERRIDES A3 above).** The
record-present/absent split and the loud `RecordIntegrityError` poison-park
are DELETED. A divergent append (same event_id, different bytes — a re-decide
after a GC'd record, or a legacy pre-0.19 terminal fact with a drifted
attempts/error payload) is resolved LOG-WINS on EVERY path: the log row is
canonical, the record is reconciled to it (`DivergentRedelivery.canonical`
carries the row), removed and re-sealed, and the append loop restarts
(byte-identical dedup-hits). Rationale: the old loud park false-parked
SUCCEEDED triggers and stormed terminal facts on every checkpoint regression —
the exact failure accept-and-advance was built to prevent. A genuinely corrupt
record loses its dedicated park but keeps a loud error log + `reactor_divergence`
observer; the log (source of truth) is untouched. Reconciliation converges
(each id reconciles at most once — log rows are immutable) and a crash in the
remove→reseal window leaves record-less state = legacy re-decide, the safe
direction.

**Backend dependency:** reconciliation needs `DivergentRedelivery.canonical`.
Memory supplies it; PG/Kurrent set `None` today → remove-only fallback (drop
the contradicted record; no lying record persists, but the body may re-run on
a later redelivery). Full PG/Kurrent reconciliation (fetch the canonical row)
is tracked as follow-up. On Kurrent-with-registry the registry-hit path never
raises divergence at all (audit #2, separate fix), so reconciliation is inert
there until that lands.

**Retention GC exempts parked records.** `remove_reclaimable` keeps a `parked`
record even when aged past the window AND behind the floor (both stores). A
park advances the ack-floor, so a parked record would otherwise become
reclaimable; GC'ing it lets a later checkpoint regression re-deliver the
trigger, and a body that now SUCCEEDS appends outputs whose event_ids are
disjoint from the terminal fact — nothing reconciles them, so both stand: the
park chimera reopened via GC. Parks are rare/exceptional, so retaining them
indefinitely is cheap and keeps the terminal outcome final. (This also makes
the DLQ observer at-most-once hold across a checkpoint regression: the record
is never lost, so redelivery always replays the park and never re-fires.)

**Known limitation of the canonical-less fallback:** when a re-decided batch
`[A', B]` diverges at `A'` on PG/Kurrent (canonical `None`), the remove-only
path does not append the later genuinely-new `B` this pass; it re-materializes
only on an external regression. Only reachable for a nondeterministic body on a
canonical-less backend; the fix is supplying `canonical` (the tracked TODO),
after which the memory reconcile path handles it uniformly. Never a chimera.

## Non-breaking note

`DecisionStore::seal` keeps its `-> Result<DecisionRecord>` signature (won
detection is a byte-compare, not a trait change), so custom backends and the
conformance suite are unaffected. `DecisionRecord` gained `parked` (via
`new()` default-false + `with_parked`), and `DivergentRedelivery` gained
`canonical` — additive fields. PG needs the `parked` column: shipped as
`CREATE TABLE` + retroactive `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (both
`parked` and, belatedly, `trigger_position`, which the 0.19 `CREATE`-only DDL
never added to pre-existing tables) and migration
`20260703_causal_decisions_parked.sql`.
