# Changelog

All notable changes to `causal-rs` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Version
numbers follow [SemVer](https://semver.org/spec/v2.0.0.html).

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
  details (not yet written — file as known gap).
