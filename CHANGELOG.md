# Changelog

All notable changes to `causal-rs` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Version
numbers follow [SemVer](https://semver.org/spec/v2.0.0.html).

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
