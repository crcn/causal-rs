# Migrating to causal 0.4

API-surface migration guide for consumers upgrading from 0.3.x to
0.4.x.

**Status:** skeleton. The shape is right; sections marked TODO need
worked examples filled in.

---

## KurrentDB-alignment rename pass (2026-05-14)

A coordinated rename made causal-rs vocabulary match KurrentDB.
Every consumer of 0.4.0-0.4.6 needs to apply this matrix when
upgrading to the post-2026-05-14 unreleased shape. See
CHANGELOG `[Unreleased]` and the design doc for full rationale.

### Rust-side identifier renames

| Find | Replace |
|---|---|
| `NewEvent` | `EventData` |
| `PersistedEvent` | `RecordedEvent` |
| `.parent_id` (field or method) | `.causation_id` |
| `EmitBuilder::parent_id(...)` | `EmitBuilder::causation_id(...)` |
| `StreamVersion` | `StreamRevision` |
| `StreamVersion::ZERO` (as `expected`) | `StreamState::NoStream` |
| `StreamVersion::from_raw(N)` (as `expected`) | `StreamState::StreamRevision(N - 1)` |
| `result.version` / `.version:` (AppendResult / RecordedEvent / Snapshot) | `.revision` |
| `backend.load_stream(...)` | `backend.read_stream(...)` |
| `backend.load_from(...)` | `backend.read_all(...)` |
| `m.get("_correlation_id")` | `m.get("$correlationId")` |
| `m.get("_parent_id")` | `m.get("$causationId")` |

### Test value shifts (1-indexed → 0-indexed)

| Was | Now |
|---|---|
| First event lands at `version = 1` | First event lands at `revision = 0` |
| Second event at `version = 2` | Second event at `revision = 1` |
| Nth event at `version = N` | Nth event at `revision = N - 1` |

### PG schema migration

`migrations/20260514_kurrent_alignment.sql` renames columns:

| Table | Old column | New column |
|---|---|---|
| `causal_log` | `parent_id` | `causation_id` |
| `causal_log` | `version` | `revision` |
| `causal_snapshots` | `version` | `revision` |

The migration is idempotent (guards on `IF EXISTS`); rerunning on
an already-migrated DB is a no-op. **It does NOT shift values** — if
your DB has v0.3-era 1-indexed `version` data, follow the value-shift
note in the migration header.

For a fresh (pre-launch) deployment like rootsignal's cutover, no
value shift is needed.

### Authoritative current schema

`docs/schema.sql` documents the post-alignment v0.4 PG schema.
`docs/schema_legacy_v0.2.sql` is the historical v0.2 queue-backed
schema, kept for reference only.

---

## What changed at a glance

| Area | 0.3 | 0.4 |
|---|---|---|
| Event identity | `Event` trait (`type_name`, `type_prefix`, `stream() -> StreamRef`) | `Fact` trait (`CATEGORY` const, `name()`, `stream_id()`) |
| `NewEvent` shape | `aggregate_type: Option<String>`, `aggregate_id: Option<Uuid>` | `category: String`, `stream_id: Uuid` (both required) |
| Aggregate | implicit via `Apply<E>` (owned) | `Aggregate` marker + `Apply<F>` (`&F` borrowed) |
| Read-side projector | `Materializer` / `MultiPrefixMaterializer` | `Projector` / `MultiProjector` |
| Reactor identity | derived | `Reactor::GROUP_NAME` const (Kurrent-aligned) |
| Engine emit | `engine.emit(fact).settled()` | `engine.emit(fact).await` (builder w/ correlation_id, metadata, expecting) |
| Engine snapshot | n/a (consumer-side fold) | `engine.snapshot::<A>(stream_id)` |
| Backend traits | `EventLog`, `ReactorQueue`, `ProjectionStore` (one Store impl) | `EventLogBackend`, `CheckpointStore`, `ReactorCheckpoint`, `SnapshotStore`, `ProjectionOps` (composable) |
| EngineBuilder | `Engine::with_store(store)` | `EngineBuilder::new(log, checkpoint, reactor_checkpoint)` (explicit at construction) |
| Macros | `#[event]`, `#[reactor]`, `#[reactors]`, `#[projection]`, singular `#[aggregator]` | `#[fact]`, `#[aggregator]`, `#[aggregators]` (legacy macros deleted) |
| DLQ | n/a (failures propagate) | `EngineBuilder::on_dlq(mapper)` |
| Global metadata | `with_event_metadata(json!(…))` | `with_default_metadata(Metadata)` |
| Ephemeral events | `NewEvent::persistent: bool` | dropped — every fact persists |

## Step-by-step

### 1. Update `Fact` impls

TODO: side-by-side of an enum Fact under 0.3 vs 0.4. Reference the
api-sharpening plan §"DX walkthrough" for the canonical shape.

```rust
// 0.3
impl Event for ScheduleEvent {
    fn type_name(&self) -> &str { /* per-variant */ }
    fn type_prefix() -> &'static str { "schedule" }
    fn stream(&self) -> StreamRef { /* ... */ }
}

// 0.4
impl Fact for ScheduleEvent {
    const CATEGORY: &'static str = "schedule";
    fn name(&self) -> &str { /* per-variant */ }
    fn stream_id(&self) -> Uuid { /* per-variant */ }
}
```

Or use the `#[fact]` macro — see `causal_core_macros` docs.

### 2. Migrate aggregates

TODO: `Apply<E>` owned → `Apply<F>` borrowed. The `#[aggregator]`
macro emits the new shape automatically; manual impls need the
`fn apply(&mut self, fact: &F)` signature.

### 3. Rename projectors

TODO: `Materializer` → `Projector`, `MultiPrefixMaterializer` →
`MultiProjector`, `CATEGORIES` const for cross-category subscription.

### 4. Reactors — declare `GROUP_NAME`

TODO. Every reactor now has a stable consumer-group identifier.
Kurrent persistent-subscription compatibility hinges on this.

### 5. Engine construction

```rust
// 0.3
let engine = Engine::new(deps, store)
    .with_reactor(/* ... */)
    .with_projection(/* ... */);

// 0.4
let engine = EngineBuilder::new(log, checkpoint, reactor_checkpoint)
    .with_aggregators(/* ... */)
    .with_reactors(/* ... */)
    .with_projectors(/* ... */)
    .with_default_metadata(/* ... */)
    .on_dlq(/* mapper */)
    .build(deps);
```

TODO: full annotated example.

### 6. `engine.emit` — drop `.settled()`, use the builder

TODO. `EmitBuilder` is `IntoFuture` so `engine.emit(fact).await`
works for the simple case. Longer form chains `.correlation_id`,
`.parent_id`, `.metadata`, `.expecting`.

### 7. Backend traits

Consumers writing custom backends:
- Implement `EventLogBackend` instead of legacy `EventLog`.
- Split queue duties: `CheckpointStore` (cursors) + `ReactorCheckpoint`
  (atomic batch commit, C12). No more single `Store` trait.
- DLQ + pause/resume + status move to `ProjectionOps` (extends
  `CheckpointStore`).
- See `causal_replay::PgEventLogBackend` / `PgReactorCheckpoint` /
  `PgSnapshotStore` for the reference Postgres implementation.

### 8. Macros

| 0.3 macro | 0.4 replacement |
|---|---|
| `#[event]` | `#[fact]` |
| `#[reactor]` | (deleted; impl `Reactor` directly or use `reactor::on::<F>()`) |
| `#[reactors]` | (deleted) |
| `#[projection]` | (deleted; impl `Projector` directly) |
| `#[aggregator]` (singular, on impl) | `#[aggregator]` (new semantics — on fn returning Aggregator) |
| n/a | `#[aggregators]` (module-level wrapper) |

### 9. Persisted-data migration

The on-disk event format changed shape (`event_type` composition,
`aggregate_type` semantics). Existing data needs a rewrite — the
general approach:

1. Build a per-Fact mapping table (old `type_prefix:variant` → new
   `CATEGORY:name`, old `aggregate_type` → new `CATEGORY`).
2. Run idempotent UPDATEs in stages.
3. Verify with distinct-pair queries.
4. Deploy 0.4 code.

Cursors in `CheckpointStore` are global log positions — they don't
need rewriting. Consumers resume at the same position with the new
format.

## Deferred / not-in-0.4

- `snapshot_every(N)` — `SnapshotStore` trait + Postgres impl exist;
  engine wiring deferred to a follow-up release.
- Multi-process projection leases — `causal_projection_cursors`
  reserves `leased_by`/`leased_until`/`fencing_token` columns; the
  coordination protocol is deferred to a separate RFC.
- Distributed reactor sharding (multiple processes sharing a
  `GROUP_NAME` for work distribution) — `GROUP_NAME` is the
  forward-compat hook; the in-process runner doesn't fan out yet.
- `KurrentEventLogBackend` — trait alignment is verified
  (`api-sharpening-plan.md` §"KurrentDB alignment"); the backend
  implementation itself is separate work.

## Known gaps in 0.4.x

- **`AggregatorRegistry::apply_event` RMW race** — documented in
  `aggregator.rs:341-370` and the 0.4.4 CHANGELOG entry. Concurrent
  caller-emit + reactor-emit on the same stream key can lose
  updates. Mitigation paths listed; no fix yet.
