# Kurrent-native consolidation

**Date:** 2026-06-07
**Status:** Phases 0, 1, 3 + the structural swap (Phase 2 merged: append-collapse;
Phase 4: ReactionCache + outbox→direct-append) all landed and **live-validated on
KurrentDB and Postgres**. Cleanup done: non-optional read fields, atomic `Vec`
batch append, `ReactorOutbox`→`ReactorCheckpoint` rename, catch-up-vs-persistent
doc honesty. Remaining: Phase 5 (2D `$all` cursor, docs) + rootsignal migration;
optional Kurrent server-side persistent subscriptions; EventData vestigial-field
removal + 2-arg builder (cosmetic follow-ups — see bottom).
**Author:** craig + Claude

## Progress log

- **Staff-bar hardening pass (2026-06-08).** Closed the open reservations:
  - **Postgres live-validated.** Stood up Postgres, applied the schema, ran the
    ignored PG suites: `pg_event_log` 5/5 (incl. C1 idempotency on
    `causal_log_event_id_key`, C6 OCC on `idx_causal_log_stream`),
    `pg_snapshot_store` 5/5, `kurrent_pg_hybrid` 1/1. Live PG caught a real
    pre-existing bug: `PgSnapshotStore` queried column `version` but the
    kurrent-alignment migration had renamed it to `revision` — fixed.
  - **Atomic multi-fact append.** `EventLogBackend::append_to_stream` now takes
    `Vec<EventData>` and commits the batch atomically (Kurrent native multi-event
    append; PG single transaction; Memory single-mutex). `Engine::append` builds
    the whole decision and appends it in one OCC call — a crash can no longer
    tear a multi-fact decision. Idempotency keys on the batch's last `event_id`.
  - **`ReactorOutbox` → `ReactorCheckpoint`** (trait had zero outbox methods);
    `PgReactorOutbox` → `PgReactorCheckpoint` + file rename; engine/runner
    `outbox` field → `reactor_checkpoint`/`checkpoint`. Removed the
    "durable backends MUST persist attempt counters" contract that contradicted
    both backends (they hold them in-memory by design).
  - **Doc honesty sweep.** Reactors/projectors documented as catch-up
    subscriptions (`read_all` from a client cursor), not Kurrent persistent
    subscriptions; purged stale `RelayLoop` / outbox-drain / `outbox_pending`
    references from `engine.rs`, `aggregator.rs`, `reactor_runner.rs`, `reactor.rs`.
- **Phase 3 hardened (pressure test).** A stress test (8-way concurrent
  unconditional appends to one OCC stream) exposed that the fixed 3-retry
  budget exhausted under contention. Fixed: `Engine::append` now uses a 16-retry
  budget with jittered exponential backoff (UUID-seeded, no new dep). 5/5
  reliable; `append_high_contention_all_increments_apply` pins it. The
  multi-fact decision test confirms contiguous revisions (0,1,2).
- **Phase 4 — ReactionCache landed + wired into the reactor path.**
  `ReactionKey` + `ReactionCache` trait + `InMemoryReactionCache` +
  `remember(...)` (`reaction_cache.rs`). Deterministic emit ids already existed
  (`reactor_runner::derive_output_event_id` + `NS_REACTOR_OUTPUT`);
  `ReactionKey::output_event_id` reuses it. Wired through the runner:
  `EngineBuilder::with_reaction_cache` → `ReactorRunner` → `Ctx`, with the
  ergonomic `ctx.remember(GROUP_NAME, || async {…})`. Proven end-to-end
  (`reaction_cache_dedups_side_effect_across_retry`): a reactor's external call
  runs **once** despite a forced retry — so it's useful *today* (idempotent
  reactor retries), not just under the future at-least-once subscription model.
- **Phase 4 — remaining (the structural swap).** Blast radius measured and large:
  streamless `append` is a convenience in ~20 test sites; the outbox/relay touches
  `engine.rs` (~47 refs), `memory_store.rs` (~44), a dedicated PG outbox test file
  (~26), the DLQ path, and `settle()`. Replacing the outbox with the typed
  persistent-subscription path (which carries `stream_id` → unblocks the merged
  Phase 2 append-collapse + `_global` removal + non-optional read fields) is a
  multi-step migration, best landed against a **live KurrentDB 26.1** so
  redelivery/ack/ordering are validated for real, not only the in-memory sim.

## Why

causal ships Kurrent-faithful *primitives* (`Event` = CATEGORY + stream_id,
`StreamState`, 0-indexed `StreamRevision`, a correct `append_to_stream` in the
Kurrent backend) but the *runtime* doesn't drive them. `Engine::emit` always
calls the non-CAS `append` (`engine.rs:838`) with `StreamState::Any`, so:

- OCC is never used on the hot path — the single most valuable thing Kurrent
  gives event sourcing is bypassed.
- The documented OCC command path (`Engine::append<A,F>`, `with_aggregate`,
  `StreamPolicy::OccRequired` — `aggregate.rs:1-15`) is **unbuilt**.
- A "streamless event" concept (the second `append`, optional
  `aggregate_type/aggregate_id`, the `{category}-_global` stream) is bolted on
  top of a model where every `Event` already has a stream.
- The processing spine is a Postgres transactional outbox (`reactor_runner.rs`
  "C12"), which reimplements — and is weaker than — Kurrent's native
  subscription machinery.

We have not shipped. Fix the design at the root, the Kurrent way.

## Locked decisions

1. **One append primitive, OCC-first.** Collapse `append` + `append_to_stream`
   into a single stream-oriented append. Expected-revision is the spine. Build
   the `Engine::append<A,F>` decider path the docs already promise. `Any` only
   for explicitly unordered fact streams.

2. **Reactors run as at-least-once catch-up consumers, not a PG outbox.**
   Remove `ReactorOutbox` / `commit_reactor_batch` / the PG reactor outbox.
   A `Reactor` is a **catch-up subscription**: the runner reads the log from a
   client-managed cursor (`read_all` from the position in `CheckpointStore`),
   calls `react()`, appends each output **directly** to its own stream with a
   deterministic `event_id`, then advances the cursor. At-least-once +
   idempotent: a crash between append and cursor-advance re-runs `react()` on
   restart; the re-append dedups on `event_id` (C1).
   - **AS BUILT — catch-up, not server-side persistent subscriptions.** The
     original draft of this decision said a Reactor would *be* a Kurrent
     persistent subscription (server checkpoints, `nack` retry, parked-message
     DLQ, competing consumers). The shipped model is catch-up over the
     `EventLogBackend::read_all` abstraction — one cursor mechanism shared by
     projectors and reactors across all three backends (Memory/PG/Kurrent),
     which is simpler and backend-uniform. DLQ is the runner's retry-budget +
     `on_dlq` mapper, not Kurrent parked messages. **Server-side persistent
     subscriptions remain a possible future Kurrent-only optimization** (it
     would move checkpoints + competing-consumer fan-out server-side) but are
     deliberately out of scope here.
   - **Projections keep the checkpoint-in-same-store cursor.** Keep
     `causal_projection_cursors`.
   - **Target confirmed: rootsignal migrates to Kurrent** (own `PostgresStore` →
     `KurrentEventLogBackend`, async reactor model). So causal optimizes for the
     Kurrent path; Postgres stays for projections/snapshots only.

3. **Unify the log/wire vocabulary on stream/category.** See the
   de-overloading note below — it is not a blind rename.

4. **Reactor idempotency model — SUPERSEDES "deterministic EventId by index".**
   The reaction is the unit, keyed by `reaction_key = (GROUP_NAME,
   trigger.event_id)` — the key rootsignal already computes
   (`web_scrape_reactor.rs:61`). The earlier "uuidv5 from output_index" idea is
   **withdrawn**: rootsignal's expensive reactors are non-deterministic (LLM /
   scrape return variable output) and mint entity ids with `Uuid::new_v4()` at
   ~88 sites, so neither output count nor ids are stable across re-execution —
   index-based ids would duplicate. Instead:
   - **Side-effecting / non-deterministic reactors (~19): durable
     reaction-keyed result cache.** Memoize the external result under
     `reaction_key`. Redelivery returns the cached result → the reaction becomes
     replayable/deterministic → emit is idempotent. Doubles as cost control
     (no duplicate $$ LLM/HTTP). rootsignal already threads `idempotency_key`
     into activities; make it durable + keyed by `reaction_key`.
     **causal owns this**: a `ReactionCache` trait (get/put by `reaction_key`)
     wired into the reactor runner so reactors are replay-safe by default;
     backends/apps supply the impl (PG / Redis / etc.).
   - **Pure/deterministic reactors (~12): deterministic EventIds** =
     `uuidv5(reaction_key, stable_local_key)` — Kurrent dedups on redelivery.
   - **Entity ids must be derived, not random.** rootsignal's 88 `Uuid::new_v4()`
     emit sites become `uuidv5(reaction_key, ...)` derivations. This is the core
     "do it right" rootsignal refactor.
   - **No separate inbox** (Kurrent's append-dedup is the inbox). **No outbox**
     (persistent subs + idempotent emit replace it). The two things that earn
     their weight: the result cache + deterministic id derivation — both lighter
     than an outbox, both already seeded in rootsignal.
   - **Exactly-once *effect* across external systems is impossible**; this gets
     effectively-once: cached side effects + idempotent emit. The only residual
     re-call window is crash between the external call and caching its result —
     benign for read-style calls; write-style sinks (Neo4j) use idempotent
     ops (MERGE).

5. **2D `(commit, prepare)` `$all` cursor.** Replace the commit-only `LogCursor`
   (`kurrent_event_log.rs:113,154,444`) wherever a catch-up read happens —
   mandatory once a command writes multiple events under one commit.

## The vocabulary de-overloading (Decision 3 detail)

`aggregate_type` / `aggregate_id` mean **two different things** today:

| Layer | `aggregate_type` | `aggregate_id` | Action |
|---|---|---|---|
| Wire / log (`EventData`, `RecordedEvent`, `EventLogBackend` params, Kurrent metadata, PG/memory log impls, conformance, mirroring, inspector log reads, `engine.rs` emit construction) | `Event::CATEGORY` | the **stream** id | rename → `category` / `stream_id` |
| Aggregate / fold (`aggregator.rs`, `snapshot_store.rs`, `Snapshot`, PG snapshot store, engine hydration/snapshot) | `Aggregate::NAME` | the aggregate **grouping** id (may differ from stream id via `for_type_with_id_fn`) | **keep** `aggregate_type` (= NAME) / `aggregate_id` |

`aggregate_id` is **not** universally the stream id — `for_type_with_id_fn`
(`aggregator.rs:163-176`) lets an aggregator key by `run_id` while the event
streams by `signal_id`. Renaming it to `stream_id` in the fold layer would be a
correctness bug. Today's single name across both layers is itself a latent
confusion; this split fixes it.

`Snapshot` keeps `aggregate_type` (= `Aggregate::NAME`) and `aggregate_id`. Drop
the `_aggregateType` Kurrent metadata key — `category` is recoverable from the
stream name `{category}-{stream_id}`.

## Phases (each leaves the tree green + conformance passing)

- **Phase 0 — doc-drift fixes** (safe, independent)
  - Remove the `$by_causation_id` claim (no such system projection): comment
    `kurrent_event_log.rs:28,366-373`, `README.md:110`.
  - Rewrite idempotency wording — there is no "~1-min EventId cache"; dedup is
    EventId-vs-stream-head best-effort under `Any`, strong under expected
    version (`kurrent_event_log.rs:18,96-100`; test `kurrent_event_log_test.rs:94-98`).
  - Docker image `kurrent/kurrentdb-ce:latest` → `kurrentplatform/kurrentdb:latest`
    (`kurrent_event_log_test.rs:15`).
  - Default connection scheme `esdb://` → `kurrentdb://` in docs/defaults
    (esdb:// still parses; it's a legacy synonym).

- **Phase 1 — de-overload vocabulary** (mechanical, per the table above; no
  behavior change; conformance proves it)

- **Phase 3 — OCC command path** (NEXT — additive, unblocked)
  - `Engine::append<A,F>(id, decide)` — load → `decide(&A) -> Result<Vec<F>>`
    (pure) → append with expected revision → bounded retry on `ConflictError`.
  - `EngineBuilder::with_aggregate<A,F>` → `StreamPolicy::OccRequired`;
    `Engine::emit` into an OCC-required category errors ("use append").
  - Uses the existing `append_to_stream(category, stream_id, expected,
    EventData)`; does NOT touch the outbox/relay. `Vec` batch deferred to the
    Phase 2+4 merge (Kurrent `batch_append`).

- **Phase 2 — collapse to one append primitive — MERGED INTO PHASE 4.**
  Discovery: the streamless `append` + `{category}-_global` exist *because* the
  reactor outbox row (`checkpoint_store.rs` `OutboxRow`) carries no `stream_id`,
  so the type-erased relay drain (`relay.rs:65`) appends reactor outputs with no
  stream identity. Deleting `append` / killing `_global` / non-optional
  `RecordedEvent.{stream_id,revision}` all require reactor outputs to be streamed
  — which the typed persistent-subscription path (Phase 4) provides natively.
  Doing it standalone would mean adding `stream_id` to the outbox row + a PG
  migration we'd delete in Phase 4. So: collapse the append primitive *with* the
  reactor swap.

- **Phase 4 — reactor model swap (the big one)**
  - Replace the PG reactor outbox with Kurrent persistent subscriptions
    (at-least-once); remove `ReactorOutbox`/`commit_reactor_batch`.
  - Reactor idempotency per Decision 4: a `reaction_key = (GROUP,
    trigger.event_id)`; a durable reaction-keyed **result cache** for
    side-effecting reactors; `uuidv5(reaction_key, ...)` ids for emits.
    No inbox, no outbox.
  - In-memory backend simulates persistent-sub semantics (at-least-once +
    ack + EventId dedup) so tests run without a live Kurrent.
  - Rework `settled()`/quiescence to a "all reactor groups' checkpoints passed
    position P" signal instead of outbox drain.
  - Ordering for aggregate-fold consumers: single/Pinned consumer (RoundRobin
    competing consumers reorder within a category).

- **Phase 5 — cursor + docs**
  - 2D `(commit, prepare)` `$all` cursor.
  - README/module docs: Kurrent = log; projections = PG checkpoint-in-store
    (idiomatic); reactors = native persistent subs. Delete the Reactor-vs-
    PersistentSubscription "divergence" note.

- **Live KurrentDB validation — backend confirmed + a real bug fixed.** Stood up
  `kurrentplatform/kurrentdb:latest` (v26.x) locally on `:2113`. Ran the ignored
  Kurrent suites against it: conformance **10/10**, integration **8/8**.
  - Confirmed the **`kurrentdb://` scheme parses** in the Rust 1.2 client (Phase 0
    default is valid on the real server).
  - **Bug caught only by the live server:** `read_stream` for a missing stream —
    real KurrentDB returns `Ok` on the initial `read_stream` call and surfaces
    `ResourceNotFound` on the first `next()` during iteration, which the backend
    only handled on the initial call → it propagated as an error instead of the
    contractual empty `Vec`. MemoryStore had no such distinction, so the
    conformance test passed in-memory and failed on real Kurrent. Fixed
    `read_stream` + `run_reconcile_scan` to treat iteration-time `ResourceNotFound`
    as end-of-stream. This is the concrete justification for gating the structural
    swap on a live server.
  - Run: `KURRENT_URL=kurrentdb://localhost:2113?tls=false cargo test -p
    causal_replay --features kurrent --test kurrent_event_log_test --test
    kurrent_event_log_conformance_test -- --ignored`.

- **Structural swap, slice 1 — reactor outputs stream to their own streams
  (kills `_global`).** Reactor outputs previously went through the type-erased
  relay with no stream identity → `{category}-_global`. Now the outbox row carries
  `category` + `stream_id` (captured from the typed `EventOutput`/`ErasedFact` at
  react time, both row-build sites incl. DLQ), and the relay appends each output
  via `append_to_stream(category, stream_id, Any, …)`. Threaded through
  `InsertableOutboxRow`/`OutboxRow`, MemoryStore + PG outbox impls, and
  `docs/schema.sql` (causal_outbox gains `category`/`stream_id`). **Proven
  end-to-end against live KurrentDB** (`reactor_output_lands_in_its_own_stream_not_global`,
  hybrid Kurrent-log + Memory-outbox): a reactor's output lands in `fetched-{id}`,
  not `_global`. This unblocks deleting the streamless `append` + `_global`
  (slice 2). NOTE: the outbox `category`/`stream_id` columns are interim — they
  go away when the outbox is replaced by direct-append (final slice).

- **Structural swap, slice 2 — collapsed to ONE append primitive.** Deleted the
  streamless `append` from `EventLogBackend` and every backend (memory, kurrent,
  PG, mirroring); `append_to_stream` is now the sole, required write method.
  Removed the Kurrent `_global` stream + `stream_name` helper. `engine.emit` now
  writes via `append_to_stream(category, stream_id, Any, …)` (OCC categories are
  rejected → `Engine::append`). Added a `causal::append_event` convenience (sugar
  over the primitive; derives stream from the event when not carried) for seeds /
  ad-hoc appends. **Fixed a latent PG bug:** `PgEventLogBackend::append_to_stream`
  now handles the `event_id` UNIQUE violation idempotently (the old `append`
  guaranteed this; the relay's crash-redelivery safety depends on it). All ~30
  test call sites + 3 test wrapper backends converted. **Validated on live
  KurrentDB:** conformance 10/10, integration 9/9 (incl. the e2e reactor test);
  full workspace suite green, clean compile. Streamless `append` is gone
  everywhere.

- **Structural swap, slice 3a — reactor execution is now direct-append (no
  outbox in the path).** `ReactorRunner::step` no longer writes outbox rows +
  `commit_reactor_batch`. It appends each output **directly** to its own stream
  via `append_to_stream(category, stream_id, Any, …)` with a deterministic
  `event_id` (`derive_output_event_id`), then advances the cursor
  (`checkpoint.set`). At-least-once + idempotent replaces the old atomic outbox
  commit: a crash between append and cursor-advance re-runs `react()` on restart;
  re-appends dedup on `event_id`. The non-matching-trigger and DLQ paths likewise
  append-directly / advance-cursor. The engine-level aggregator fold (so
  `engine.snapshot` sees reactor outputs — the `ChainCount` test) is preserved by
  threading the engine registry into `ReactorRunner` (factory now 2-arg). The
  outbox/relay code still exists but is **dead** (no writers) — slice 3b deletes
  it. Reworked 5 reactor_runner tests + the crash-injection tests to the new
  model (incl. a new `reactor_append_then_checkpoint_crash_redelivers_idempotently`
  proving crash recovery; surfaced + fixed a self-trigger-discipline violation in
  a fixture). `settle()` is unchanged (the outbox just stays empty). **Validated
  live:** workspace green; Kurrent conformance 10/10, integration 9/9 (the e2e
  reactor test now exercises direct-append).

- **Structural swap, slice 3b — the outbox/relay subsystem is deleted.** Removed
  `RelayLoop` (file + module + engine relay supervisor), the
  `commit_reactor_batch`/`outbox_pending`/`outbox_delete` methods from
  `ReactorOutbox` (now just `CheckpointStore` + DLQ attempt counters — name kept
  for API stability), the `InsertableOutboxRow`/`OutboxRow` structs, the
  MemoryStore + PG outbox impls + fields, the `causal_outbox` table (schema), the
  `pg_reactor_outbox_test`, and `settle()`'s empty-outbox wait (`settle` now keys
  purely off consumer cursors + log positions, which is sufficient since outputs
  append directly). Workspace check + full suite green; clippy clean; **live
  Kurrent** conformance 10/10, integration 9/9. Decision #2 ("no outbox;
  at-least-once + idempotent reactors") is now fully realized — Kurrent is the
  log, the reactor path is direct-append, and the only coordination primitive is
  the `ReactionCache`.

- **Cleanup pass — read-side fields are now non-optional.**
  `WriteResult.revision` and `RecordedEvent.{category, stream_id, revision}` went
  `Option<_>` → concrete (every event is streamed now, so the invariant is in the
  type). `RecordedEvent::category()` is a thin accessor over the field. **Dropped
  `_aggregateType`** entirely: the Kurrent backend recovers `category`/`stream_id`
  by parsing the stream name `{category}-{trailing-36-char-UUID}` (no metadata
  round-trip); the PG backend reads its columns (defaulting defensively).
  Removed the now-dead `EventDataRevisionExt`. ~40 call/construction/assertion
  sites updated across both crates + inspector. **Live-validated** (conformance
  10/10, integration 9/9 on a fresh container — the strict stream-name parser
  correctly fails loud on legacy `_global` pollution, which a clean causal-owned
  Kurrent never produces). Remaining cleanup: `Vec` batch on `append_to_stream`
  (atomic multi-fact commands) + a stale-comment sweep in `engine.rs`.

## Audit findings (resolved into the decisions above)

Attacked the design with rootsignal as the adversary. Resolved:
- **No multi-stream atomic append in the Rust 1.2 client** (only `append_to_stream`
  + single-stream `batch_append`) → can't commit outputs+marker atomically across
  streams → marker-gating is out; the **result cache + deterministic ids** model
  wins instead.
- **Non-deterministic reactors** (LLM/scrape) can't be made replayable by ids
  alone → the durable **reaction-keyed result cache** is what earns its weight.
- **88 `Uuid::new_v4()` emit sites** in rootsignal → entity ids must become
  `uuidv5(reaction_key, …)` for idempotent emit.

Remaining sharp edges to honor while building Phase 4:
- `settled()` semantics under async persistent subs (checkpoint-watch primitive).
- In-memory sub fidelity vs real at-least-once (test/prod parity).
- `$ce-{category}` requires `--run-projections=All` (else filtered `$all`).
- Competing-consumer reordering vs ordered aggregate folds (Pinned/single consumer).
- Crash-between-call-and-cache re-call window (benign for reads; MERGE for writes).

## Downstream — rootsignal migration (target: Kurrent)

rootsignal is on causal **0.4.4 + postgres** across 6 crates (world, scout,
scout-supervisor, api, common, graph): **41 reactors** (~19 side-effecting:
LLM/HTTP/Neo4j), ~280 event types, 1 real OCC aggregate (`PipelineState`),
synchronous `.settled()` throughout, and its **own** `PostgresStore` impl.

Migration is large and spans: 0.4.4→0.5.x gap + postgres→Kurrent backend +
async reactor model + the id-derivation refactor (88 sites) + side-effect
memoization. Sequence rootsignal *after* causal Phases 2–4 land. Heaviest crate:
`rootsignal-scout`.

## Deferred cosmetic follow-ups (zero behavior change)

Consciously deferred — each is a clean, low-risk, standalone change with no
correctness impact, kept out of the marathon consolidation to avoid churn:

- **Remove `EventData::category` / `EventData::stream_id`.** The stream identity
  is now carried by the `append_to_stream(category, stream_id, …)` parameters;
  the same fields on `EventData` are written-but-ignored (the backend uses the
  params). Removing them (~30 `EventData {…}` literals) plus making
  `append_event` take explicit `(category, stream_id)` (~41 callers) drops the
  last vestigial state and the one "magical" derivation in the write path.
- **Collapse `EngineBuilder::new` to two args** (`log`, `checkpoint`). The DLQ
  retry-attempt counters are in-memory on every backend, so they can move onto
  `ReactorRunner`; `ReactorCheckpoint` then collapses into `CheckpointStore` and
  the third builder arg (currently the same store passed twice) disappears.
  Ripple: ~51 `EngineBuilder::new` call sites — purely mechanical.
