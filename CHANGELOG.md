# Changelog

All notable changes to `causal-rs` are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Version
numbers follow [SemVer](https://semver.org/spec/v2.0.0.html).

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
