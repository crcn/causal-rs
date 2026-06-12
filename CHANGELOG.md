# Changelog

All notable changes to `causal-rs` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Version
numbers follow [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### 0.10 step 4 — the deterministic Ctx (2026-06-12, breaking)

`react()` gets no ambient world: the `Ctx` is the only door, and every
door is deterministic-or-memoized (Primitive 2). Replay is byte-stable
when all three hold — injected clock, effects memoized, canonical
payloads.

- **`ctx.derive_id(label)`**: mints a deterministic identity —
  `v5(consumer ∥ trigger event_id ∥ label)` — so redelivery re-creates
  the same entity instead of a duplicate. Never `Uuid::new_v4()` in a
  consumer body.
- **`ctx.time()`** (was `ctx.now()`): the trigger's recorded time, the
  only reachable time. Append-time `created_at` stays envelope-only.
- **`ctx.effect(label, || async { .. })`** (replaces `ctx.remember` /
  `ctx.effect_key`): memoizes an external call under
  `(consumer, trigger, label)` — the consumer name comes from the
  runner, not a hand-passed string, and the label distinguishes
  multiple effects per reaction. `EffectKey` is now
  `{ consumer, trigger_event_id, label }`. Projection-store queries
  are effects too (nondeterministic under redelivery by construction).
- **Duplicate `derive_id`/`effect` labels in one invocation are a
  runtime error** (domain class): the second call would silently
  replay the first's result.
- **Effect-store floor-GC**: entries exist only to make redelivery
  deterministic, so the runner deletes them once the durable ack-floor
  passes their trigger (`EffectStore` gained `remove`); without GC the
  cache grows to the size of the log. Exception: parked terminal
  failures keep theirs — failure replay happens after the floor has
  passed and must restore the original effects, or replay re-runs them
  fresh and poisons itself.
- **Calendar-clock injection**: `EngineBuilder::with_clock`
  (`Clock` / `SystemClock` / `FixedClock`). Every framework-stamped
  timestamp (`created_at` fallback, terminal-failure record times,
  observer attempt times) comes from it; liveness waits stay on tokio
  time (the two-clock rule).
- **Builder wiring is order-independent**: consumer factories now
  receive the builder's *final* configuration at `build()` —
  previously `.with_reactor(r).on_terminal_failure(..)` silently
  dropped the mapper (and observer/effect-store/clock likewise),
  because registration captured a snapshot. An ordering-dependent
  lying default, found by this step's own tests.
- **Divergence errors print the first differing JSON path**
  (`outputs[1].candidates[0].signal_id`), not just "payload differs" —
  the nondeterminism is usually in a dependency far from the reactor.
  (`causal::event_log::first_diff_path`, shared with backends.)
- **The clippy guardrail** ships as documented config (module docs on
  `causal::contexts`): `disallowed-methods` for `Uuid::new_v4` /
  `Utc::now` in application crates. Rust can't make nondeterminism
  inexpressible; paved road (this Ctx) + guardrail (the lint) +
  backstop (the divergence check).
- **Breaking**: `ctx.now()` → `ctx.time()`; `ctx.remember(group, f)` →
  `ctx.effect(label, f)`; `ctx.effect_key` deleted; `EffectKey` shape
  changed (`group` → `consumer`, + `label`); `EffectStore` impls must
  add `remove`.

### 0.10 step 3 — the retry taxonomy (2026-06-12, breaking)

BLOCKING-2: blanket bounded-retry amplifies outages (a ten-minute
Neo4j blip mass-parks every graph-touching trigger into a manual
replay incident); blanket infinite-retry wedges partitions forever.
Neither default survives. Errors now carry a declared class, and the
terminal-failure path is **mandatory**.

- **`causal::transient(e)` / `causal::poison(e)` / `causal::domain(e)`**
  (`causal::failure`): wrap errors in reactor bodies —
  `.map_err(causal::transient)?`. The outermost classification in the
  anyhow chain wins; `.context(...)` doesn't disturb it.
  - *Transient* (connection refused, 5xx, 429): capped backoff up to a
    **liveness-time ceiling** (6h of tokio time — virtualizable under
    `start_paused`; chrono never participates), then parks as
    `transient_exhausted` — mass-replayable as a class. The attempt
    budget does not apply.
  - *Poison*: parks immediately — deterministic, retry is pointless.
    Trigger-deserialization failures and OCC-fence violations are
    structurally poison.
  - *Domain*: bounded attempts (`with_max_attempts`, default 3), then
    parks as `domain`.
  - *Unclassified* `anyhow::Error`: domain policy, parked labeled
    `unclassified` — honest, not masqueraded as a declared class.
- **The terminal path is mandatory** (Primitive 5 flip): without an
  `on_terminal_failure` mapper, the runner appends the built-in
  `causal:reaction_failed { consumer, trigger_id, trigger_event_type,
  class, error, attempts }` fact to the **trigger's own subject
  history**, so a completion fold over that subject folds the failure
  as completion-with-error. Retry-forever no longer exists;
  transient-class backoff is the sanctioned wait-out-the-outage
  behavior.
- **`TerminalFailure` carries the trigger's `subject` / `subject_id`
  and the `FailureClass`** — the mapper can stamp its fact with the
  identity completion folds need (the old `DlqInfo` lacked it).
- **Breaking**: a failing no-mapper reactor parks (built-in fact)
  after 3 attempts instead of retrying forever; `with_max_attempts`
  now applies with or without a mapper; `TerminalFailure` gained
  `subject` / `subject_id` / `class`.

### 0.10 step 2 — the partitioned reactor runner (2026-06-12, breaking)

Settle unhostaged: reactors execute concurrently across declared
partitions, one wedged trigger blocks only its own partition, and
`settled()` waits only on its own workflow's work. Landed as one
correctness unit (runner + BLOCKING-1 + BLOCKING-3 + BLOCKING-4) per
the design doc's sequencing, gated by a red-first acceptance suite
(`tests/partitioned_runner_acceptance.rs`).

- **`Reactor::ORDERING`** (BLOCKING-4, decided in
  `docs/plans/2026-06-12-memo-partition-key.md`): no global partition
  key — each reactor declares `Ordering::PerSubject` (default; one
  subject's triggers in log order, across workflows), `PerWorkflow`
  (run-pipeline shape), or `None` (per-event concurrency for
  commutative work).
- **The runner** (`reactor_runner.rs` rewritten): per-consumer
  dispatcher, one worker task per active partition (spawned on demand,
  evicted when drained). Failures retry worker-local with capped
  backoff; terminal-failure parking and the OCC output fence are
  unchanged; a poison or failing trigger without a mapper wedges only
  its partition (block-until-fixed, scoped).
- **Ack-floor checkpoint** (BLOCKING-3): the durable cursor is the
  highest position with no unacked matching trigger at or below it;
  ingestion pauses at the pending-work window (4096 outstanding
  triggers). Crash redelivery replays at most the window — never the
  log distance behind a slow partition — and never silently skips an
  unfinished trigger. Redelivered reactions dedup byte-identically on
  their identity-keyed event_ids.
- **Fold-on-read state** (BLOCKING-1): `ctx.state_of` in reactors is
  an async position-bounded fold of the subject history from the log —
  deterministic at the trigger's position regardless of partition
  interleaving; exclusive over the trigger's own subject under
  `PerSubject`. Worker-local incremental cache, correct by
  append-only construction, dies with its partition. The per-consumer
  scan-folded registry for reactors (and its startup hydration scan)
  is deleted; serial projectors keep registry semantics. Cross-subject
  fan-in aggregators (custom `id_fn`) get a teaching error on this
  path — fold them in a projector-maintained read model.
- **Settle rewire**: `Engine::settle` polls per-consumer
  `drained(workflow, hw)` probes (for reactors: ingestion scanned to
  `hw` ∧ no queued/in-flight trigger of that workflow) instead of
  every consumer's global durable cursor.
- **Breaking**: `ctx.state_of` is `async` and returns `Result`;
  reactors no longer execute in serial total order (by design — that
  order was the settle hostage); `Engine::shutdown` halts partition
  workers (in-flight reactions complete, new attempts stop).
- Scale-checked with the `nuke` harness: emit ~260k ev/s flat to
  500k; chase flat to 200k; settle_flood 128-concurrent mean settle
  ≈ 2× the 50ms drained-poll interval, no collapse.

### 0.10 step 1 — the naming + shape decoupling (2026-06-12, breaking)

Every identity is declared, named for meaning, and matched exactly.
One mechanical ripple, landed before any new machinery (see
`docs/plans/2026-06-12-design-0.10-no-lying-defaults.md`):

- **Facts**: `#[causal::event(name = "...", subject_id = "...",
  subject = "...")]` — `name` is required (the wire `event_type`,
  verbatim; never derived from the type name); the enum form is
  retracted (one fact = one struct; a family enum's new variant
  poisoned every deployed consumer); subject omission is shape-gated
  (no Uuid fields = legal; candidates present = teaching error that
  lists them; `no_subject` for reference-carrying facts).
- **Routing is flat**: `Event::NAME` replaces `CATEGORY` +
  `event_type()`; matching is equality (prefix collisions
  unconstructable; vocabulary growth additive); `MultiProjector::KINDS`
  replaces `CATEGORIES`; colons are just characters.
- **Output dedup is identity-keyed**: `derive_output_event_id` =
  v5(consumer ∥ trigger ∥ kind ∥ subject ∥ nth-of-that-pair) — stable
  under output reordering/insertion across deploys (was positional).
- **Workflow vocabulary**: `correlation_id` → `workflow_id` everywhere
  domain-facing (envelope, `EmitResult`, `Ctx`, settle tracker,
  inspector + UI). Storage keeps its native names (PG column, Kurrent
  `$correlationId`).
- **Consumers**: `GROUP_NAME` → `NAME` (all three traits).
- **Reads**: `ctx.state_of` / `engine.state_of` (were
  `aggregate_of`/`load_aggregate`); `engine.snapshot()` and
  `ctx.aggregate()` deleted.
- **Failure vocabulary**: `on_terminal_failure` + `TerminalFailure`
  (were `on_dlq`/`DlqInfo`); DLQ jargon removed.
- **Effects**: `EffectStore`/`EffectKey`/`with_effect_store` (were
  ReactionCache names).
- **Invariants**: `Aggregate::INVARIANT = true` installs the OCC fence
  via ordinary `with_aggregators` registration; `with_aggregate`
  deleted.

---

Bug-fix + scale pass (2026-06-12). Four real defects found by an
adversarial design review, fixed test-first, plus a quadratic→linear
rework of `MemoryStore`'s hot paths after a million-event assault.

### Changed (breaking)

- **`#[causal::event]` requires an explicit stream identity.** Omitting
  `stream_id` used to silently default `stream_id()` to `Uuid::nil()`,
  routing every value of every entity into one `{category}-nil` stream —
  the trap that mass-produces fan-in aggregates no per-stream read can
  serve. It is now a compile error whose message teaches the fix; a
  genuinely streamless fact (telemetry, ops counters) opts in with the
  new `nil_stream` flag. Migration: add `stream_id = "<the Uuid field
  your aggregates fold by>"`, or `nil_stream` where the singleton shape
  is intended.

  Anti-stair-step guarantees, so fixing the new error never raises a
  second one: `occurred_at()` is now generated **only when the field is
  present** (on the struct, or on ALL enum variants — absent everywhere
  = the trait default `None`; mixed presence across variants errors
  naming the odd variant out, since that's almost always a typo); a
  typo'd `stream_id`/`occurred_at_field` on a struct is a teaching
  error at the macro instead of a raw "no field" rustc error inside
  generated code. (Previously the `stream_id` path *required* an
  `occurred_at` field on every variant; enums without one simply could
  not use `stream_id`.)
- **`MemoryStore::global_log()` returns a read-only guard** instead of
  `&Mutex<Vec<RecordedEvent>>`. The store now maintains derived indices
  over that Vec, so external mutation would silently desync them — the
  guard makes it impossible by construction. Migration: drop the
  `.lock()` call (`store.global_log().lock().len()` →
  `store.global_log().len()`); the guard derefs to `[RecordedEvent]`.
- **Divergent redeliveries are rejected loudly** (all backends:
  MemoryStore, Postgres, Kurrent). A dedup-hit on `event_id` whose
  `payload` / `event_type` / `correlation_id` / `causation_id` differs
  from the persisted row now errors instead of silently keeping the old
  row while reporting success — a differing re-emission means the
  producer is nondeterministic under redelivery (wall clock, rand, or
  an external call not under `ctx.remember`), and the old behavior let
  state diverge invisibly from intent. Byte-identical redelivery
  (modulo `created_at`/`metadata`, which legit redeliveries re-stamp)
  still collapses to the original `WriteResult`. New conformance
  scenario: `divergent_redelivery_is_rejected`.

### Fixed

- **Multi-fact `emit` could tear.** `emit(vec![a, b])` appended one
  fact at a time with fresh `event_id`s per call: a mid-batch failure
  left a torn prefix durably in the log, and the caller's natural
  retry-after-`Err` duplicated it. Consecutive same-stream facts now
  land as **one atomic batch** (all-or-nothing), so a failed emit
  leaves nothing behind and retrying cannot duplicate. Cross-stream
  batches remain atomic per same-stream run, in input order — the
  backend primitive is per-stream; the `emit` docs now state the
  contract. The OCC-required-category guard also moved ahead of any
  write, so a rejected fact mid-batch can no longer leave earlier
  facts behind.
- **`MemoryStore` was quadratic; now linear.** Every append paid an
  O(N) stream-count scan + O(N) dedup scan, and every `read_all` poll
  scanned from the log's front — at 100k events, emit throughput had
  already fallen 15× (58k → 3.9k ev/s), and a million-event log was
  minutes of pure scanning. `read_all` now binary-searches the
  position-ordered log; appends use derived indices (`event_id` →
  offset, stream → offsets). Measured after: flat ~260k emits/s through
  2M events (7.7s total), 1M-event projector catch-up in 0.4s. The
  public `global_log()` accessor is unchanged.
- **Stale flake note on antifragile attack 8.** The "fails ~1/20 by
  losing a fold" NOTE described pre-remediation behavior; the
  vacant-restore TOCTOU it pointed at was fixed 2026-06-10 (monotonic
  `set_state`). Verified 2026-06-12 with 0 failures across 600 runs
  (release + debug); the comment now records the fix instead of
  re-reporting the bug to every reader.

### Added

- `examples/nuke.rs` — scale-assault harness (`emit` / `drain` /
  `settle_flood` scenarios with per-decile timing reports).
- trybuild UI tests for the `#[event]` macro (missing `stream_id`,
  struct + enum forms, `nil_stream` conflict and opt-in).

---

Audit-remediation pass (2026-06-10). Fixes two data-integrity bugs,
several cross-backend contract divergences, and a large dead API
surface; adds CI. Pre-1.0, so breaking changes ship now while there is
no production data. Step-by-step upgrade: [`docs/MIGRATION_0.8.md`](docs/MIGRATION_0.8.md).

The core backend/consumer trait contracts — `EventLogBackend`,
`CheckpointStore`, `ReactorCheckpoint`, `SnapshotStore`, `Event`,
`Aggregate`, `Reactor`, `Projector`, `MultiProjector` — are
**unchanged**; nothing you implement breaks.

### Changed (breaking)

- **`EngineBuilder::build()` is now `async` and returns `Result`**:
  `build(self) -> Engine` → `async fn build(self) -> Result<Engine>`.
  It seeds fresh reactor cursors at `latest_position()` and validates
  categories before returning, both of which require I/O. Migration:
  `builder.build()` → `builder.build().await?`. This is the only change
  that touches normal application code.
- **`ctx.aggregate` / `ctx.aggregate_of` / `Engine::snapshot` /
  `Engine::load_aggregate` now panic** (naming the aggregate type) when
  the aggregate was never registered, instead of silently returning
  `A::default()` forever. A type that never folds is a configuration
  bug, and silently defaulting it shipped a real consumer's dedup gates
  that never fired. Register the aggregator, or remove the read.
- **`EngineBuilder::with_observer`** takes `Arc<dyn ReactorObserver>`
  instead of a generic `Arc<O>`. Source-compatible: a concrete
  `Arc<MyObserver>` coerces at the call site.
- **Reactor / projector trigger routing is colon-aware.** A bare
  `event_type.starts_with(category)` let category `"order"` match
  `"orders:created"` and feed a foreign payload to the trigger
  deserializer; matching now requires the `{category}:` boundary. Only
  affects prefix-colliding category names.
- **A `:` in `Event::CATEGORY` is rejected at `build()`.** The colon is
  the `{category}:{name}` separator; one inside a category silently
  desynced reactor matching from aggregate folding. Now a loud build
  error. Rename such categories to be colon-free.
- **`REPLAY` env var is parsed strictly** (`causal_replay`): only `1`
  / `true` enable replay; `0` / `false` / empty / unset stay live (the
  old `is_ok()` treated `REPLAY=0` as replay-on).
- **OCC decider keys streams on `STREAM_CATEGORY`** (C3). `Engine::load`
  / `Engine::append` previously placed and read the decider stream by
  `F::CATEGORY`; they now use `F::STREAM_CATEGORY` (matching `emit` and
  durable restore), fold only `F`-typed events from a co-located stream,
  and take the expected revision from the true stream head.
  `stream_name_for::<F>` likewise returns `{STREAM_CATEGORY}-{id}`.
  Only observable when an event type overrides `STREAM_CATEGORY`.
- **Reactors are fenced out of OCC-required categories** (C4). A reactor
  that emits a fact whose category was registered via `with_aggregate`
  is now rejected (routed to the DLQ) — its `Any` append could not have
  upheld the aggregate's optimistic-concurrency invariant. Model such a
  write as an `Engine::append` command.
- `AggregatorRegistry::apply_event` and `replay_events_onto` changed
  signature (module-path-public engine internals; not in the prelude —
  normal code never calls these directly).

### Removed (breaking — all were exported but wired to nothing)

- `Upcaster`, `UpcasterRegistry` — never consulted by any read path.
  Re-introduce wired when the first schema change needs it (read-side,
  no storage migration).
- The projection-ops surface: `ProjectionMode`, `RetryPolicy`,
  `Backoff`, `FailureBehavior`, `ProjectionOps`, `ProjectionStatus`,
  `ProjectionFailure` — configurable but read by no runner.
  `StartPosition` is **kept** (now wired, reactor-only).
- Legacy `#[reactor]`, `#[reactors]`, `#[projection]` proc-macros and
  the `DistributedSafe` derive (~1,650 lines generating calls to APIs
  that no longer exist). `#[event]`, `#[aggregator]`, `#[aggregators]`
  are **kept**.
- `AggregatorRegistry::replay_events` (dead), `capture_for_rollback`,
  `restore_state` (the deleted rollback machinery).

### Added

- **`causal::event_type`** module — the single owner of the
  `{category}:{name}` format: `compose`, `category_of`,
  `matches_category`, `validate_category`.
- **`EngineBuilder::with_reactor_start(reactor, StartPosition)`** — seed
  a reactor's cursor explicitly. Plain `with_reactor` defaults to
  `ResumeOrLatest` (resume a persisted cursor, else start at
  `latest_position()` — a fresh reactor does not re-fire side effects
  for history); `StartPosition::Zero` is the opt-in to process history.
- **`causal_replay`**: `reconcile` / `Reconciliation` (the shared,
  full-batch redelivery-vs-conflict decision used by every backend) and
  `ADVISORY_LOCK_CLASS` / `ADVISORY_LOCK_OBJID` constants.
- **CI** (`.github/workflows/ci.yml`): clippy, the unit suite,
  `cargo test --doc`, and the `--ignored` Postgres + KurrentDB +
  hybrid conformance suites against a dockerized stack.
- New conformance scenarios: concurrent-appender gap-freedom,
  expected-ahead rejection, concurrent-`Any` appends,
  redelivery-after-foreign-write, partial-overlap rejection.

### Fixed

- **Postgres `read_all` silent data loss (critical).** `position` is a
  `BIGSERIAL` assigned at insert; transactions commit out of order, so
  a tailer could checkpoint past a still-uncommitted lower position and
  skip that event forever. Appends now take a transaction-scoped
  `pg_advisory_xact_lock`, making commit order == position order;
  `latest_position()` is trustworthy as a result.
- **Fold/checkpoint corruption (critical).** Aggregate folds desynced
  from checkpoints on transient checkpoint-set failure, crash
  redelivery, and the DLQ path — and snapshots then persisted the
  corruption at inflated revisions. Folds are now idempotent on the
  event's stream coordinates ("fold tracks the log, not body success");
  the rollback machinery that caused the DLQ desync is deleted.
- **Vacant-entry restore TOCTOU.** Concurrent cold-registry folds to a
  stream with history could lose folds (and, rarely, desync
  permanently) because read-through restore installed state
  unconditionally after an async tail read. `set_state` is now
  monotonic; `repair_gap` no longer advances past a concurrently-gapped
  event.
- **Postgres accepted an expected revision ahead of the stream head**,
  silently punching revision holes; now validated and rejected with a
  typed `ConflictError` (matching Kurrent + MemoryStore).
- **Concurrent `Any`/`StreamExists` appends** no longer spuriously
  conflict on Postgres (the advisory lock removes the read-MAX-then-
  insert race).
- **Within-batch duplicate `event_id`s** are rejected by `MemoryStore`
  (matching the durable backends' `UNIQUE(event_id)`) instead of
  persisting both rows and breaking dedup.
- **Kurrent `Any` appends are now fully idempotent on `event_id`** (B3).
  Kurrent's native best-effort dedup only compares against the stream
  head, so a redelivery racing a foreign write could duplicate; `Any`
  appends now scan-then-CAS (reusing the shared `reconcile`) to honor the
  trait contract absolutely, the same guarantee Postgres and MemoryStore
  give. The reactor hot path and the already-correct backends are
  untouched.
- **Poison-pill triggers route to the DLQ instead of wedging** (B4). A
  trigger payload that can't deserialize into `R::Trigger` was propagated
  before the retry budget engaged, blocking the cursor forever. It now
  routes to the DLQ immediately (deterministic — retries never help) when
  a mapper is configured, or propagates (block-until-fixed) without one.
- **DLQ retry-budget ordering**: `clear_reactor_attempts` moved to after
  the synthesized append + cursor advance, so a failing DLQ append no
  longer resets the budget each step (livelock) and a crash mid-DLQ no
  longer replays a full `react()` retry cycle.
- **Kurrent reconcile** verified the batch *tail* event_id only; now
  verifies every id in the batch (the shared `reconcile`).
- **docs.rs front pages**: the `causal` example called a nonexistent
  `Engine::builder`; `causal_replay`'s page described only the legacy
  replay API and the wrong version. Both rewritten and `cargo test
  --doc`-checked.

## [0.7.4] - 2026-06-09

### Added — durable aggregate restore (read-through, revision-based)

Folded aggregate state now survives a process restart. Previously the
engine-level registry (`Engine::snapshot`) was only ever folded by live emits,
so after a restart an aggregate whose events sat behind the consumers'
checkpoints returned empty/partial state.

- **`EngineBuilder::with_snapshot_store(Arc<dyn SnapshotStore>)`** wires durable
  restore. **`with_snapshot_every(n)`** sets the snapshot cadence (default 100;
  `0` disables saving — restore still works via full replay). Without a store,
  behavior is unchanged.
- **`Engine::load_aggregate::<A>(id).await -> Option<A>`** — the async,
  restart-surviving counterpart to the sync `snapshot` peek. Read-through: loads
  the snapshot (if any), replays the tail of the aggregate's stream, folds, and
  caches. A snapshot blob that fails to deserialize self-heals (deleted, rebuilt
  from genesis). `snapshot` stays a sync in-memory peek.
- Snapshots are saved every N folded events, keyed by the aggregate stream's
  revision (never a `$all` position), so they round-trip with `read_stream`.
- Inside a consumer body, `ctx.aggregate::<A>(id)` already survives restart via
  the existing genesis hydration; this release adds the engine-level path and
  snapshot acceleration.

### Added — `Event::STREAM_CATEGORY` (stream placement vs routing)

- New `Event::STREAM_CATEGORY` const (defaults to `CATEGORY`) controls the
  stream an event is *stored* in (`{STREAM_CATEGORY}-{id}`), independent of
  `CATEGORY`, which stays the consumer/aggregator **routing** key. This lets
  several distinct event types co-locate in one stream (so a single aggregate
  can be restored from one `read_stream`) while keeping distinct routing
  categories. Default = unchanged (each event streams by its own category).
- `Aggregate::STREAM_CATEGORY` const (defaults to `""` = restore disabled)
  declares the stream an aggregate folds from.
- `#[event(..., stream = "…")]` macro attribute emits `STREAM_CATEGORY`
  (additive; distinct from the legacy `stream_category` attribute, which still
  sets `CATEGORY`).
- `MemoryStore` now implements `delete_snapshot` (was the no-op default), so
  self-heal actually removes a bad snapshot.

All additive — no public breakage. With no snapshot store wired and no
`STREAM_CATEGORY` set, behavior is identical to 0.7.3.

## [0.7.3] - 2026-06-09

### Docs (correctness)

- `Engine::settle`'s rustdoc described the pre-0.7.2 global-head algorithm; 0.7.2
  changed the behavior but not the doc. Rewritten to document the
  per-correlation high-water algorithm, the single-engine correctness boundary,
  and the shared-consumer-cursor caveat at the call site. Docs-only; no behavior
  change from 0.7.2.

## [0.7.2] - 2026-06-09

### Changed — `Engine::settle` is now per-run (correlation-scoped)

`settle(result)` previously ignored `result` and waited for every consumer to
reach the *global* log head. In a one-engine/many-runs deployment under
continuous emission, that head never stops moving, so a run's `settle` could
wait on unrelated runs forever. (The `result` parameter had been an unused stub
since 0.5.0; this is its first real implementation — no prior per-run settle
existed to regress.)

`settle` now waits only for the causal chain of `result.correlation_id`:

- The engine keeps an in-process per-correlation high-water mark. Each reactor
  runner records its output's `$all` position under the trigger's
  `correlation_id` (outputs already inherit it), so the whole chain shares one
  key. `settle` waits until every consumer is past that mark and no new
  chain event has appeared — then the run has drained, regardless of other
  runs' traffic.
- Floored at the emit position, so `settle` always waits for consumers to at
  least observe the trigger (and empty-emit `settle` keeps its
  drain-to-current-head behavior).
- Bounded: entries are removed when `settle` returns; a hard cap evicts under
  fire-and-forget load. No new public API, no trait changes — purely additive.

Correctness boundary: the high-water is in-process, so this is correct when a
run's reactors execute in the same engine instance that called `settle` (the
single-engine deployment). A multi-engine deployment sharing one log would need
a backend-queried high-water (not implemented).

`emit(e).correlation_id(run_id).settled()` therefore becomes per-run
automatically — no caller change.

## [0.7.1] - 2026-06-08

### Added

- `DlqInfo` now carries `correlation_id` — the failing trigger's run / causal
  chain. The DLQ-synthesized event already inherited it; exposing it on the
  struct lets an `on_dlq` mapper key its terminal-failure event per-run (e.g.
  stream-by-`run_id`) so a dead-letter can still unblock that run's downstream
  gates instead of leaving them waiting. Additive — `DlqInfo` is only
  constructed by the runtime and handed to mappers, so existing consumers are
  unaffected.

## [0.7.0] - 2026-06-08

### Added — Postgres observability (best-effort, fleet-wide inspector backend)

The inspector previously read from an in-process `MemoryStore`, so its event
flow, reactor logs, and chain-of-events were visible only on the box that
processed them — useless across a load-balanced fleet, and lost on restart.
This release adds a Postgres-backed observability store so any box can serve the
full picture.

KurrentDB remains the durable source of truth. Postgres is a deliberately
**best-effort, lossy** read + observability store — never used for coordination,
leasing, or anything that has to be bulletproof. Three new public types in
`causal_replay` (all under the `postgres` feature):

- **`PgReactorObserver`** — implements `causal::ReactorObserver`. On the reactor
  hot path the hooks only `try_send` to a bounded channel (drop-on-overflow); a
  background writer batches reactor executions, logs, descriptions, and
  aggregate snapshots into Postgres in one transaction of idempotent UPSERTs.
  DLQ folds to `status = 'dead_letter'`. Cheap on the write path, lossy by
  design.
- **`PgInspectorReadModel`** — implements all of `causal_inspector`'s
  `InspectorReadModel`. Event/flow views query `causal_log`; observability views
  join it on `event_id` so there is a single Postgres sequence authority.
- **`PgEventProjector`** — a background catch-up consumer that mirrors the source
  log's `$all` into `causal_log` with `ON CONFLICT (event_id) DO NOTHING`.
  Idempotent, so any/all boxes can run it and a restart simply resumes.

### Added — schema

- `migrations/20260608_reactor_observability.sql` (also in the canonical
  `docs/schema.sql`): `causal_reactor_executions`, `causal_reactor_logs`,
  `causal_reactor_descriptions`, `causal_aggregate_snapshots`. The aggregate and
  description tables key on `(event_id, …)` so the at-least-once reactor firehose
  collapses to one row per `(event, key)`.

### Changed

- `examples/inspector-demo` now runs on the production-shape stack — KurrentDB as
  the source of truth, Postgres as the observability backend — instead of the
  in-process `MemoryStore` mirror. Its `docker-compose.yml` gains a Postgres
  service seeded from `docs/schema.sql`. The flow graph and chain-of-events are
  derived from Postgres and survive restarts.

### Hardening (pre-release audit)

- `PgInspectorReadModel::reactor_outcomes` now reports the **terminal** status of
  each reactor — a reactor that failed then recovered reads as `completed`, not
  `failed` — and pairs the error with that terminal attempt (so a recovered
  reactor shows no error). Matches the `MemoryStore` reference; the previous
  worst-status logic mislabeled every recovered reactor as failed.
- `reactor_attempt_history` returns only closed attempts; in-flight `running`
  rows no longer render as zero-duration completed attempts.
- `PgEventProjector` writes a non-aggregate event's identity as `NULL` rather
  than `("", nil, 0)`, preserving the `causal_log` all-set-or-all-NULL invariant
  and avoiding a partial-unique-index collision that `ON CONFLICT (event_id)`
  cannot catch (which would otherwise stall the projector).
- All correlation-scoped reads are bounded (`LIMIT 10000`) so a pathological
  correlation can't load an unbounded result set into a single inspector request.

## [0.6.0] - 2026-06-08

### Breaking — atomic batch append

- `EventLogBackend::append_to_stream` now takes `Vec<EventData>` instead of a
  single `EventData` and commits the batch atomically (KurrentDB native
  multi-event append; Postgres single transaction; `MemoryStore` single mutex).
  `Engine::append` builds the whole decision and appends it in one OCC call, so
  a crash can no longer tear a multi-fact decision. Single-event callers pass
  `vec![event]`. Idempotency keys on the batch's last `event_id`.

### Breaking — rename

- `ReactorOutbox` → `ReactorCheckpoint`, `PgReactorOutbox` → `PgReactorCheckpoint`
  (`reactor_outbox.rs` → `reactor_checkpoint.rs`). The trait never had any outbox
  methods — it carries the per-consumer cursor plus DLQ retry-attempt counters.
  No behavior change.

### Fixed

- **OCC retry was silently broken on the durable backends.** `PgEventLogBackend`
  and `KurrentEventLogBackend` returned a bare `anyhow!` string on an
  expected-version mismatch instead of the typed `ConflictError` the trait
  contract and `Engine::append`'s retry loop require — so a concurrent-write
  conflict surfaced as a hard error instead of triggering reload-redecide-retry.
  Both backends now return `ConflictError`; the conformance suite asserts the
  *type* (not the message) so it can't regress.
- `PgSnapshotStore` queried column `version`, but the Kurrent-alignment migration
  had renamed it to `revision` — the Postgres snapshot path was broken. Caught by
  running the (previously never-executed) ignored PG suite against live Postgres.
- `PgEventLogBackend` read aggregate identity defensively, silently defaulting a
  half-populated row to the nil stream at revision 0; it now errors on a
  half-populated `(aggregate_type, aggregate_id, revision)`.
- Removed `migrations/20260206_add_dead_letter_queue.sql` — it referenced a
  `causal_events` table that never existed in causal and created an unused table
  (rootsignal-v0.3 drift that broke a clean migration run). See
  `migrations/README.md`: `docs/schema.sql` is the canonical fresh-install schema.

### Docs (correctness)

- Rewrote `modules/causal/README.md` — its quick-start described the pre-0.4 API
  (`Fact`, `Materializer`, old `append` signature, `ReactorOutbox`, `causal =
  "0.3"`) and would not compile. Now matches the current `Event` / `Reactor` /
  `Engine::append` API.

### Docs

- Reactors and projectors are documented accurately as **catch-up subscriptions**
  (client-managed cursor via `read_all`), not Kurrent server-side persistent
  subscriptions. Purged stale `RelayLoop` / outbox-drain references throughout.

## [0.5.0] - 2026-05-15

### Breaking — module-path cleanup (the _v3 → canonical rename)

The three migration-era `_v3` modules drop their suffix; the legacy
non-v3 versions they coexisted with had already been deleted earlier
in the v0.4 line. Direct imports via the old paths break.

- `causal::aggregate_v3` → `causal::aggregate` (trait + Apply<F>)
- `causal::engine_v3` → `causal::engine` (Engine, EngineBuilder, EmitBuilder)
- `causal::reactor_v3` → `causal::reactor` (Reactor, Events, EventOutput)

Public re-exports through `causal::Engine`, `causal::Reactor`,
`causal::Aggregate`, `causal::Events`, etc. are unchanged — only
direct `causal::<module>_v3::` paths need updating to the new module
name.

### Docs housecleaning

Removed historical implementation plans, brainstorms, audits, and
migration-era schema/runbooks from `docs/` (~55 files). What remains:
`README.md`, `CHANGELOG.md`, `docs/MIGRATION_0.4.md`,
`docs/schema.sql`. Source-tree doc headers stripped of phase-numbered
migration narration (v0.3, v0.4, "Phase 4d MVP", "Lives at X until
Phase 9 renames the file", etc.) — the surface IS current, so the
docs say so directly.

### Breaking — final KurrentDB vocabulary cleanup (2026-05-15)

Follow-up to the 2026-05-14 alignment pass. Closes the remaining
divergences after one more audit:

- **`Fact` trait → `Event`.** Closes the last causal-only term in
  the core surface. The three-layer model is now
  `Event` (typed authoring trait) → `EventData` (write boundary) →
  `RecordedEvent` (read boundary). `causal::Event` replaces
  `causal::Fact`; module path renamed `causal::fact` →
  `causal::event`; `pub use causal_core_macros::event` is the macro
  (was effectively unchanged — the trait was the noun).
- **`Event::name()` → `Event::event_type()`.** Method name matches
  the stored field on `EventData` / `RecordedEvent`. No more
  `f.name() → "{prefix}:{name}"` recursion.
- **`AppendResult` → `WriteResult`.** Kurrent's exact name on the
  return type from `append` / `append_to_stream`.
- **Dropped `load_stream` / `load_from` aliases** from
  `EventLogBackend`. `read_stream` / `read_all` are the only names;
  every backend impl + caller uses them.

Migration (additive to the 2026-05-14 matrix in this same
`[Unreleased]` block):

| Find | Replace |
|---|---|
| `Fact` (trait) | `Event` |
| `Fact::CATEGORY` | `Event::CATEGORY` |
| `impl Fact for X` | `impl Event for X` |
| `fn name(&self) -> &str` (on Event impls) | `fn event_type(&self) -> &str` |
| `.name()` (on Event values) | `.event_type()` |
| `causal::fact::*` | `causal::event::*` |
| `AppendResult` | `WriteResult` |
| `.load_from(...)` | `.read_all(...)` |
| `.load_stream(...)` | `.read_stream(...)` |

### Breaking — KurrentDB vocabulary alignment

A coordinated rename pass to make causal-rs feel native to KurrentDB
developers. Every shape that has a Kurrent counterpart now uses
Kurrent's exact name; deliberate divergences (`Fact` vs `Event`,
`Reactor` vs `PersistentSubscription`) are documented in `README.md`
under "KurrentDB vocabulary mapping".

- **`NewEvent` → `EventData`** (write-side struct). Mirrors
  `kurrentdb::EventData`.
- **`PersistedEvent` → `RecordedEvent`** (read-side struct).
  Mirrors `kurrentdb::RecordedEvent`.
- **`parent_id` → `causation_id`** on both structs and on
  `EmitBuilder::causation_id(...)`. Kurrent's universal term for
  "the event that caused this one." Underlying PG column renames
  in a separate migration; see MIGRATION_0.4.md.
- **Metadata reserved keys** moved to KurrentDB's `$`-prefix
  convention: `_correlation_id` → `$correlationId`,
  `_parent_id` → `$causationId`. **This unlocks server-side
  projections** (`$by_correlation_id`, `$by_causation_id`) — they
  read those exact keys. Causal-internal keys (`_persistent`,
  `_aggregateType`) keep the `_` prefix to distinguish from
  Kurrent system metadata.
- **`StreamVersion` → `StreamRevision`** (0-indexed). First event
  in a stream now has revision 0, matching Kurrent exactly. The
  off-by-one translation layer in `KurrentEventLogBackend` is
  gone — kurrentdb revision and causal::StreamRevision are
  identity-mapped.
- **New `StreamState` enum** for the `expected` parameter on
  `EventLogBackend::append_to_stream`. Matches
  `kurrentdb::StreamState` variant-for-variant: `Any`, `NoStream`,
  `StreamExists`, `StreamRevision(u64)`. Callers writing the FIRST
  event to a stream pass `StreamState::NoStream` (was
  `StreamVersion::ZERO`).
- **`AppendResult.version` → `AppendResult.revision`** and
  **`RecordedEvent.version` → `RecordedEvent.revision`** and
  **`Snapshot.version` → `Snapshot.revision`**.
- **`EventLogBackend::load_stream` → `read_stream`** and
  **`load_from` → `read_all`**. Kurrent's verbs. Old names kept
  as default-method aliases until a future major release.
- **`Engine::append_to_stream(fact)`** added as an alias for
  `Engine::emit(fact)` — familiar entry point for Kurrent devs.
- **`causal::stream_name_for::<F>(id)`** and
  **`causal::event_type_for(&fact)`** helpers — pure functions
  that compose the canonical stream name (`{CATEGORY}-{id}`) and
  event_type (`{CATEGORY}:{name}`) the runtime would use. Useful
  for out-of-band Kurrent access.

Migration matrix:

| Find | Replace |
|---|---|
| `NewEvent` | `EventData` |
| `PersistedEvent` | `RecordedEvent` |
| `.parent_id` | `.causation_id` |
| `EmitBuilder::parent_id(...)` | `EmitBuilder::causation_id(...)` |
| `StreamVersion::ZERO` (as `expected`) | `StreamState::NoStream` |
| `StreamVersion::from_raw(N)` (as `expected`) | `StreamState::StreamRevision(N - 1)` |
| `result.version` | `result.revision` |
| `AppendResult { version: ... }` | `AppendResult { revision: ... }` |
| `Snapshot { version: ... }` | `Snapshot { revision: ... }` |
| `.load_stream(...)` | `.read_stream(...)` |
| `.load_from(...)` | `.read_all(...)` |
| `m.get("_correlation_id")` | `m.get("$correlationId")` |
| `m.get("_parent_id")` | `m.get("$causationId")` |

Test value shifts (1-indexed → 0-indexed):

| Was | Now |
|---|---|
| First event lands at `version = 1` | First event lands at `revision = 0` |
| Second event at `version = 2` | Second event at `revision = 1` |

The conformance suite covers all of this — every backend impl runs
through identical scenarios against `StreamState::NoStream` +
`StreamRevision::ZERO`.

### Added

- **`causal_replay::KurrentEventLogBackend`** behind a new `kurrent`
  feature flag. Implements `EventLogBackend` against KurrentDB
  (formerly EventStoreDB) via the official `kurrentdb = "1.2"` crate.
  The trait surface was designed against Kurrent's primitives; this
  is the implementation.

  Locked design decisions are documented inline:
  - **CAS append**: causal's 1-indexed `StreamVersion` (ZERO = empty
    stream, first event lands at v1) is translated to Kurrent's
    0-indexed revision (`NoStream` for fresh stream, `StreamRevision(N-1)`
    for an N-event stream). Three unit tests pin the conversion
    (`causal_zero_maps_to_no_stream`,
    `causal_v1_maps_to_kurrent_revision_0`,
    `causal_vN_maps_to_kurrent_revision_N_minus_1`) plus
    `kurrent_revision_round_trips_through_causal_version` cross-checks
    against the kurrentdb crate's own test fixture (3 events written
    → `next_expected_version=2`). On `WrongExpectedVersion` the
    backend scans the conflict slice for the caller's `event_id` and
    returns the existing AppendResult on duplicate (idempotent retry)
    or surfaces an `OCC conflict` error on a real collision.
  - **Non-CAS append** uses `StreamState::Any` with `EventData::id`.
    Kurrent's ~1-min EventId cache dedups within the window; post-
    cache retries can produce duplicates. Documented gap on
    `KurrentEventLogBackend::append`.
  - **Stream naming**: aggregate events → `{type}-{id}`, non-aggregate
    → `{category}-_global` (`_global` isn't a valid UUID, so it can't
    collide).
  - **Metadata** maps to Kurrent's `custom_metadata` slot; reserved
    keys `_correlation_id` / `_parent_id` / `_aggregate_type` /
    `_persistent` are stamped on write and stripped on read.

  Scope is the event log only. `CheckpointStore`, `ReactorOutbox`,
  and `SnapshotStore` keep using the Postgres backends — Kurrent is
  an event store, not a job queue (hybrid Option B). Multi-process
  leases, snapshots-as-events, catch-up subscriptions, cross-node
  sync are all out of scope here.

  Integration tests live in `tests/kurrent_event_log_test.rs` and
  are `#[ignore]`'d by default — run against a live KurrentDB on
  `KURRENT_URL` with `cargo test --features kurrent -- --ignored`.
  Pure-function unit tests (stream-name composition, metadata
  stamping) run unconditionally.

### Fixed

- **`AggregatorRegistry::apply_event` is now atomic per key.** The
  RMW that read pre-state, cloned, applied, and inserted post-state
  ran across separate DashMap operations. Two concurrent callers on
  the same stream key — typically `Engine::execute_emit`
  (caller-emit) racing `RelayLoop::drain_once` (reactor-emit) —
  could both read the same pre-state; the second insert overwrote
  the first; the earlier event's mutations were lost. Documented as
  a known-issue in 0.4.4. The fix holds a DashMap `entry()` guard
  for the full read-modify-write block, serializing concurrent
  applies on the same key.

  A regression test
  (`aggregator_apply_event_serializes_concurrent_callers`,
  `engine_v3.rs`) pins the contract: 8 threads × 200 applies on the
  same stream id. Pre-fix loses ~70% of updates in release mode;
  post-fix the final fold count equals total applies.

  The `:prev` slot remains best-effort (written outside the entry
  guard — holding it inside could deadlock if `key` and `:prev`
  hash to the same DashMap shard). Readers that need the exact
  per-event transition should consume `TransitionSnapshots`
  returned by `apply_event`; the on-disk `:prev` slot is documented
  racy and only kept for backward compatibility.

### Documentation

- **`Reactor::react` doc** clarifies that output facts can be from
  any category — not just the trigger's. Cross-category reactor
  outputs are the common shape (e.g. a lifecycle reactor that emits
  scheduling facts). Closes pressure-test finding F9.5.
- **`StartPosition::Zero` doc** carries a loud replay-hazard warning
  with the application-side mitigations (blue-green via
  `ProjectionStream::Mode::Replay`, idempotent UPSERTs, coordinated
  downtime). `StartPosition::Specific` references the same warning
  when the position is behind a downstream consumer. Closes
  pressure-test finding F6.2.

### Tests

- New regression test
  `projector_batch_sees_per_event_prev_curr_interleaved`
  (`engine_v3.rs`) pins the apply→project interleaving contract for
  `ProjectionRunner::step`. Batches of N events must fold each into
  the aggregator registry and *then* invoke `project()` for that
  event, before moving on — not fold-all-then-project-all. Without
  this contract, transition guards reading
  `ctx.aggregate::<A>().prev` vs `.curr` would silently see the
  same `(prev, curr)` pair for every batch element. Closes
  pressure-test finding F9.6.

## [0.4.6] — 2026-05-12

### Fixed (follow-up to 0.4.5)

- **`#[aggregators]` (no args) now expands bare functions** using
  `Fact::stream_id`. Pre-0.4.6, a module marked `#[aggregators]` with
  no module-level default (`singleton` / `id` / `id_fn`) would silently
  skip any `fn` items that lacked a per-function `#[aggregator]` attr.
  The skip was invisible — the `aggregators()` factory returned an
  empty Vec and the engine appeared healthy until a snapshot returned
  None at the expected key.
- **`#[aggregator]` (no args) is now valid** with the same default
  (`Fact::stream_id`). Required for scout's pipeline_aggregators where
  most fact types use the natural per-fact stream.

### Migration from 0.4.5

If your `#[aggregators(singleton)]` module previously had no
per-function attrs, the singleton attr was load-bearing — pre-0.4.5
it was a no-op (every aggregator hard-coded `Fact::stream_id`),
0.4.5 made it actually singleton (`Uuid::nil()` key). 0.4.6 keeps
the 0.4.5 semantics for that case. If you wrote `#[aggregators(singleton)]`
intending the legacy (silent stream_id) behavior, drop the `singleton`
attr → `#[aggregators]` — now valid in 0.4.6 and means "default to
Fact::stream_id".

## [0.4.5] — 2026-05-12

### Fixed (breaking-ish — see migration note)

- **`#[aggregator(id_fn = "...")]` / `#[aggregator(id = "...")]` /
  `#[aggregator(singleton)]` now actually work.** From 0.4.0 through
  0.4.4 the macro accepted these attributes but the generated factory
  hard-coded `Fact::stream_id`, so the per-aggregator key extraction
  documented in the v0.3 docs was silently a no-op. Aggregators
  registered with `id_fn` quietly folded into the same key as
  `Fact::stream_id`, which collapsed multi-aggregator-per-fact setups
  onto a single key.

  Two regression tests pin the new behavior
  (`aggregator_for_type_with_id_fn_keys_independently`,
  `macro_aggregator_id_fn_actually_keys_by_method`). Verified to detect
  the prior bug by reverting the macro change.

  **Migration:** If you were on 0.4.0–0.4.4 with `id_fn`/`id`/`singleton`
  attributes, the aggregator was probably folding incorrectly under
  the hood. On 0.4.5 it folds correctly. If your application depended
  on the incorrect (singleton-collapsed) behavior, expect aggregate
  state to redistribute across stream keys after this upgrade. Tests
  that asserted state at `Uuid::nil()` may now see state at the
  intended key instead.

### Added

- **`Aggregator::for_type_with_id_fn<A, F, IdFn>(id_fn: IdFn)`**: public
  API for the new id-extraction path. `id_fn: Fn(&F) -> Option<Uuid>`;
  returning `None` skips the fold for this aggregator on that fact
  (third regression test:
  `aggregator_id_fn_returning_none_skips_fold`).
- **`AggregatorIdValue` trait** with impls for `Uuid` and `Option<Uuid>`
  so the macro accepts user methods returning either shape.

## [0.4.4] — 2026-05-12

### Added

- **Test:** `engine_v3::tests::engine_snapshot_sees_reactor_emitted_facts`
  pins the 0.4.3 relay→aggregator-fold contract. Future refactors of
  `RelayLoop` that omit the `apply_event` call now fail loudly instead
  of silently regressing every consumer using `engine.snapshot()` with
  cross-fact aggregates.
- **Docs:** Concurrency caveat documented on
  `AggregatorRegistry::apply_event`. The read-modify-write against
  DashMap is not atomic per key; concurrent applies from
  `Engine::execute_emit` and `RelayLoop::drain_once` can lose updates
  on the same stream key. Mitigation paths are listed in the doc
  comment but not yet implemented (tracked as known-issue).

### Changed (technically — 0.4.3 behavior, redocumented)

The 0.4.3 release introduced the relay-side aggregator fold but
shipped without test coverage or a CHANGELOG entry. Recapping for
consumers who upgraded from 0.4.2:

- `engine.snapshot::<A>(stream_id)` now reflects **both** caller-
  emitted facts (folded by `execute_emit`, as in 0.4.2) and reactor-
  emitted facts (folded post-`log.append` by `RelayLoop`). Prior to
  0.4.3, reactor outputs only updated each consumer's private
  registry clone and were invisible to out-of-band readers.
- This is a behavior change for any consumer that built around the
  prior "snapshot sees only caller-emit" semantics. If that was
  load-bearing for you, query consumer-side state via `ctx.aggregate`
  inside the consumer instead.

## [0.4.3] — 2026-05-12

### Changed

- `RelayLoop` folds reactor-emitted facts into the engine aggregator
  registry after `log.append`. See 0.4.4 "Changed" note above for
  the user-visible impact. Ships without dedicated test coverage —
  remedied in 0.4.4.

## [0.4.2] — earlier

### Fixed

- `#[aggregator]` macro emits the v0.4-correct `Apply` impl shape
  (`fn apply(&mut self, fact: &F)` with internal clone-shadow for
  legacy v0.3-style owned bodies) and `Aggregator::for_type::<A, F>()`
  factory.

## [0.4.1] — earlier

### Fixed

- `PgReactorOutbox` implements the full `ReactorOutbox` trait
  (`record_reactor_attempt`, `clear_reactor_attempts` added).

## [0.4.0] — earlier

### Changed (breaking)

- New v0.4 API surface: `engine.emit(fact)`, `engine.snapshot::<A>(id)`,
  Reactor/Projector traits with `GROUP_NAME` consts, `ReactorObserver`,
  `EngineBuilder` with three explicit backend traits (`EventLogBackend`,
  `CheckpointStore`, `ReactorOutbox`). See `docs/MIGRATION_0.4.md` for
  the consumer migration guide.
