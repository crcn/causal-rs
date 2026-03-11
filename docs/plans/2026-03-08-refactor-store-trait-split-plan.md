---
title: "refactor: Split Store into EventLog + ReactorQueue"
type: refactor
date: 2026-03-08
brainstorm: docs/brainstorms/2026-03-08-kurrentdb-store-composition.md
---

# Split Store into EventLog + ReactorQueue

## Overview

Replace the monolithic `Store` trait with two focused traits: `EventLog` (durable
event history) and `ReactorQueue` (work distribution). This eliminates the
redundant event buffer (`causal_events` queue), halves event writes, and enables
swapping `PostgresEventLog` for `KurrentDbEventLog` with zero other changes.

## Problem Statement

The `Store` trait does three jobs:

1. **Event buffer** — `publish`/`poll_next`/`complete_event` (transient queue)
2. **Event log** — `append_event`/`load_stream`/`load_global_from` (durable history)
3. **Reactor queue** — `poll_next_handler`/`resolve_handler` (work distribution)

Job 1 is redundant. Every event passes through both the buffer AND the log — two
copies, two writes. With a checkpoint cursor, the log itself can be the source
for the settle loop.

## Proposed Solution

Kill the event buffer. Two traits remain: `EventLog` and `ReactorQueue`.
The engine holds both, reads new events from the log via checkpoint, creates
reactor intents via the queue.

See brainstorm for full design rationale and pressure-tested issues.

## Design Decisions (from brainstorm)

These were resolved during the brainstorm phase. Not re-litigating here:

- **Correlation-primary streams** for KurrentDB (Strategy A)
- **Routing metadata** (`_hops`, `_batch_id`, etc.) in `NewEvent.metadata`
- **In-memory event retry counter** (KurrentDB subscription model)
- **Log before resolve** atomicity (emitted events appended before reactor complete)
- **Total idempotency contract** on `EventLog::append`
- **Aggregate matching before append** (cheap JSON extraction, no IO)
- **Engine-side ephemeral cache** (not on IntentCommit)
- **Version-guard for aggregator idempotency** (separate fix, not blocking)

## Clarifications (from spec-flow analysis)

These gaps were surfaced by analysis and need explicit answers:

### C1: `IntentCommit::park()` and `::skip()` advance the checkpoint

Both `park` and `skip` set `commit.checkpoint = event.position`, advancing past
the event. Without this, parked/skipped events create infinite retry loops.
`park` additionally records a DLQ entry. `skip` is a no-op commit (empty intents,
no DLQ, just checkpoint advancement).

### C2: `QueueStatus.pending_events` removed

`ReactorQueue` cannot compute "events past checkpoint" without access to
`EventLog`. Remove `pending_events` from `QueueStatus`. If the engine needs it
(e.g. for the admin panel), it computes it: `log.load_from(queue.checkpoint(), MAX).len()`.

### C3: Ephemeral field on `NewEvent` and `PersistedEvent`

Add `ephemeral: Option<Arc<dyn Any + Send + Sync>>` to both `NewEvent` and
`PersistedEvent`. `MemoryStore::append` copies it through. Postgres/KurrentDB
ignore it (always `None` on load). The engine-side ephemeral cache is populated
from `PersistedEvent.ephemeral` during Phase 1 of settle. For Postgres, reactors
JSON-deserialize — same as today for replayed events. Acceptable.

For same-process dispatch optimization: `dispatch()` stores the ephemeral on
`NewEvent`. `MemoryStore.append` preserves it. `load_from` returns it. Cache is
populated. No regression for the common (test) case.

Custom `Debug` impl needed since `Arc<dyn Any>` doesn't implement `Debug`.

### C4: `Engine::new` signature

```rust
pub fn new(
    deps: D,
    log: Arc<dyn EventLog>,
    queue: Arc<dyn ReactorQueue>,
) -> Self
```

Helper for tests:
```rust
impl<D: Send + Sync + 'static> Engine<D> {
    pub fn in_memory(deps: D) -> Self {
        let store = Arc::new(MemoryStore::new());
        Self::new(deps, store.clone(), store)
    }
}
```

Most tests use `Engine::new(deps)` today (default MemoryStore). These become
`Engine::in_memory(deps)` — a find-and-replace.

### C5: `MemoryStore` persistence always on

No more `persistence_enabled` flag. The event log is always active (it IS the
source of events now). `MemoryStore::with_persistence()` removed.
`MemoryStore::new()` is the only constructor.

### C6: Batch metadata round-trip

Engine writes `_batch_id`, `_batch_index`, `_batch_size` to `NewEvent.metadata`
in `build_new_event`. During Phase 1, engine reads them from
`PersistedEvent.metadata` and populates `ReactorIntent.batch_id/index/size`.
Same flow as `_hops`.

### C7: Projection execution timing

Projections run during Phase 1 of settle, inside `process_event`. Same timing as
today. `IntentCommit.projection_failures` captures any failures.

### C8: Reactor descriptions in `enqueue`

`IntentCommit.handler_descriptions` is persisted atomically with intents inside
`queue.enqueue()`. No separate `set_descriptions` call needed during settle.
The `set_descriptions`/`get_descriptions` methods on `ReactorQueue` remain for
external callers (admin panel, ProcessHandle).

### C9: `EventOutcome` and rejection removed

No event buffer means no event rejection. The in-memory retry counter + `park`
mechanism replaces `EventOutcome::Rejected`. `EventOutcome` type removed entirely.

### C10: Enqueue failure vs process failure

The spec-flow analyzer noted that `event_attempts` conflates "process failed"
with "enqueue failed." This is acceptable: if `enqueue` fails transiently, the
retry counter increments but the event will succeed on the next attempt. Only
persistent failures exhaust retries. The counter resets on process restart.

## Technical Approach

### File Changes Summary

| File | Action |
|---|---|
| `src/event_log.rs` | **New** — `EventLog` trait |
| `src/handler_queue.rs` | **New** — `ReactorQueue` trait |
| `src/types.rs` | **Modify** — add `IntentCommit`, `EventPark`; add `ephemeral` to `NewEvent`/`PersistedEvent`; remove `QueuedEvent`, `EventCommit`, `EventOutcome`, `pending_events` from `QueueStatus`; remove `events_to_publish` from `HandlerCompletion`/`HandlerDlq` |
| `src/store.rs` | **Remove** |
| `src/memory_store.rs` | **Modify** — implement `EventLog` + `ReactorQueue`; remove event buffer; add checkpoint; remove persistence flag |
| `src/engine.rs` | **Modify** — `log` + `queue` fields; rewrite settle loop; rewrite dispatch; ephemeral cache; `in_memory()` helper |
| `src/job_executor.rs` | **Modify** — `Arc<dyn ReactorQueue>` for journaling; `process_event` returns `IntentCommit` |
| `src/process.rs` | **Modify** — `Arc<dyn ReactorQueue>` |
| `src/reactor/context.rs` | **Modify** — `Arc<dyn ReactorQueue>` for `JournalState` |
| `src/event_store.rs` | **Modify** — `&dyn EventLog` for helper functions |
| `src/lib.rs` | **Modify** — update re-exports |
| `tests/engine_integration.rs` | **Modify** — update all tests |

### Implementation Phases

#### Phase 1: New traits and types (additive, nothing breaks)

**Goal:** Add all new types and traits alongside existing ones. Zero test breakage.

##### 1a. New types in `types.rs`

```rust
// crates/causal/src/types.rs

/// Atomic intent creation payload (replaces EventCommit).
pub struct IntentCommit {
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    pub intents: Vec<ReactorIntent>,
    pub projection_failures: Vec<ProjectionFailure>,
    pub handler_descriptions: HashMap<String, serde_json::Value>,
    pub checkpoint: u64,
    pub park: Option<EventPark>,
}

impl IntentCommit {
    /// Park an event in DLQ and advance checkpoint past it.
    pub fn park(event: &PersistedEvent, reason: impl Into<String>) -> Self { ... }

    /// Skip an event (advance checkpoint, no intents, no DLQ).
    pub fn skip(event: &PersistedEvent) -> Self { ... }
}

/// Reason for parking (DLQ-ing) an event.
pub struct EventPark {
    pub reason: String,
}
```

Add `ephemeral: Option<Arc<dyn Any + Send + Sync>>` to `NewEvent` and
`PersistedEvent`. Custom `Debug` impl for both (skip the ephemeral field).

Remove `pending_events` from `QueueStatus`.

##### 1b. `EventLog` trait in `event_log.rs`

```rust
// crates/causal/src/event_log.rs

#[async_trait]
pub trait EventLog: Send + Sync {
    async fn append(&self, event: NewEvent) -> Result<AppendResult>;
    async fn load_from(&self, after_position: u64, limit: usize) -> Result<Vec<PersistedEvent>>;
    async fn load_stream(&self, aggregate_type: &str, aggregate_id: Uuid, after_version: Option<u64>) -> Result<Vec<PersistedEvent>>;
    async fn load_snapshot(&self, _t: &str, _id: Uuid) -> Result<Option<Snapshot>> { Ok(None) }
    async fn save_snapshot(&self, _s: Snapshot) -> Result<()> { Ok(()) }
}
```

##### 1c. `ReactorQueue` trait in `handler_queue.rs`

```rust
// crates/causal/src/handler_queue.rs

#[async_trait]
pub trait ReactorQueue: Send + Sync {
    async fn enqueue(&self, commit: IntentCommit) -> Result<()>;
    async fn checkpoint(&self) -> Result<u64>;
    async fn dequeue(&self) -> Result<Option<QueuedReactor>>;
    async fn earliest_pending_at(&self) -> Result<Option<DateTime<Utc>>>;
    async fn resolve(&self, resolution: HandlerResolution) -> Result<()>;
    async fn reclaim_stale(&self) -> Result<()> { Ok(()) }

    // Journaling
    async fn load_journal(&self, reactor_id: &str, event_id: Uuid) -> Result<Vec<JournalEntry>>;
    async fn append_journal(&self, reactor_id: &str, event_id: Uuid, seq: u32, value: Value) -> Result<()>;
    async fn clear_journal(&self, reactor_id: &str, event_id: Uuid) -> Result<()>;

    // Coordination
    async fn cancel(&self, correlation_id: Uuid) -> Result<()> { Ok(()) }
    async fn is_cancelled(&self, correlation_id: Uuid) -> Result<bool> { Ok(false) }
    async fn status(&self, correlation_id: Uuid) -> Result<QueueStatus> { Ok(QueueStatus::default()) }
    async fn set_descriptions(&self, correlation_id: Uuid, d: HashMap<String, Value>) -> Result<()> { Ok(()) }
    async fn get_descriptions(&self, correlation_id: Uuid) -> Result<HashMap<String, Value>> { Ok(HashMap::new()) }
}
```

##### 1d. Update `lib.rs` re-exports (additive)

Add `pub mod event_log;` and `pub mod handler_queue;`. Re-export new traits.
Keep old re-exports for now.

**Checkpoint:** `cargo test` passes. New traits and types exist alongside old ones.

---

#### Phase 2: MemoryStore implements new traits (additive)

**Goal:** `MemoryStore` implements both `EventLog` and `ReactorQueue`. Old `Store`
impl stays temporarily.

##### 2a. `impl EventLog for MemoryStore`

Map existing methods:
- `append` ← current `append_event` (always enabled, no persistence flag)
- `load_from` ← current `load_global_from`
- `load_stream` ← current `load_stream`
- `load_snapshot` ← current `load_snapshot`
- `save_snapshot` ← current `save_snapshot`

Store `ephemeral` from `NewEvent` on `PersistedEvent` in the in-memory log.

##### 2b. `impl ReactorQueue for MemoryStore`

New field: `checkpoint: Arc<AtomicU64>` (initialized to 0).

Map existing methods:
- `enqueue` — new: create reactor intents + advance checkpoint atomically
- `checkpoint` — new: return current checkpoint value
- `dequeue` ← current `poll_next_handler`
- `earliest_pending_at` ← current `earliest_pending_handler_at`
- `resolve` ← current `resolve_handler` (but `HandlerCompletion`/`HandlerDlq` no longer have `events_to_publish` — resolution does NOT publish events)
- `reclaim_stale` ← current `reclaim_stale`
- Journaling methods ← unchanged
- Coordination methods ← unchanged

The `enqueue` implementation replaces the old `complete_event(EventOutcome::Processed(...))`.
It takes an `IntentCommit`, creates `QueuedReactor` entries for each intent,
handles `park` (DLQ recording), and advances the checkpoint.

**Critical change in `resolve`:** Currently `resolve_handler(Complete(...))` takes
`events_to_publish: Vec<QueuedEvent>` and inserts them into the event buffer.
In the new design, `resolve` does NOT publish events — the engine appends to the
log before calling resolve. `HandlerCompletion` and `HandlerDlq` no longer have
`events_to_publish`.

This means the NEW `resolve` impl for MemoryStore is simpler: just mark the reactor
complete/retry/dlq. No event publishing.

##### 2c. Remove `persistence_enabled`

Remove the flag, `with_persistence()`, and the conditional logic. The event log
is always active. `MemoryStore::new()` is the only constructor.

**Checkpoint:** `cargo test` passes. Both old `Store` and new `EventLog`/`ReactorQueue`
impls coexist on MemoryStore.

---

#### Phase 3: Switch engine and all consumers (breaking)

**Goal:** Engine uses `EventLog` + `ReactorQueue`. Old `Store` trait removed.
All tests updated.

This is the big bang. Do it in one commit — there's no useful intermediate state
where the engine half-uses the old Store and half-uses the new traits.

##### 3a. Engine struct changes

```rust
// crates/causal/src/engine.rs

pub struct Engine<D: Send + Sync + 'static> {
    log: Arc<dyn EventLog>,
    queue: Arc<dyn ReactorQueue>,
    deps: Arc<D>,
    reactors: Arc<HandlerRegistry<D>>,
    aggregators: Arc<AggregatorRegistry>,
    // ... (unchanged: upcasters, projections, global_dlq_mapper, etc.)
}

impl<D: Send + Sync + 'static> Engine<D> {
    pub fn new(
        deps: D,
        log: Arc<dyn EventLog>,
        queue: Arc<dyn ReactorQueue>,
    ) -> Self { ... }

    /// Convenience: in-memory engine for tests.
    pub fn in_memory(deps: D) -> Self {
        let store = Arc::new(MemoryStore::new());
        Self::new(deps, store.clone(), store)
    }
}
```

Remove `with_store()`. Builder methods (`.with_reactor()`, `.with_aggregator()`,
etc.) unchanged.

##### 3b. Rewrite settle loop

Follow the brainstorm pseudocode. Key structure:

```rust
pub async fn settle(&self) -> Result<()> {
    let mut event_attempts: HashMap<u64, u32> = HashMap::new();
    let mut ephemerals: HashMap<Uuid, Arc<dyn Any + Send + Sync>> = HashMap::new();

    loop {
        let mut did_work = false;
        self.queue.reclaim_stale().await?;

        // ── Phase 1: Process new events from the log ──
        let checkpoint = self.queue.checkpoint().await?;
        let events = self.log.load_from(checkpoint, BATCH_SIZE).await?;

        let mut cancelled_cache = HashSet::new();

        for event in events {
            did_work = true;

            // Cache ephemeral for Phase 2
            if let Some(eph) = &event.ephemeral {
                ephemerals.insert(event.event_id, eph.clone());
            }

            // Cancellation check
            if /* cancelled */ {
                self.queue.enqueue(IntentCommit::skip(&event)).await?;
                continue;
            }

            // Hop check
            let hops = event.metadata.get("_hops")...;
            if hops >= self.max_hops {
                self.queue.enqueue(IntentCommit::park(&event, "exceeded max hops")).await?;
                continue;
            }

            // Event-level retry
            let attempts = event_attempts.entry(event.position).or_insert(0);
            *attempts += 1;
            if *attempts > self.max_event_attempts {
                self.queue.enqueue(IntentCommit::park(&event, ...)).await?;
                event_attempts.remove(&event.position);
                continue;
            }

            // Process: hydrate aggregates, apply, match reactors, run projections
            match self.process_event(&event).await {
                Ok(commit) => {
                    self.queue.enqueue(commit).await?;
                    event_attempts.remove(&event.position);
                }
                Err(e) => { break; }
            }
        }

        // ── Phase 2: Execute reactors ──
        let mut executions = vec![];
        while let Some(mut h) = self.queue.dequeue().await? {
            // Inject ephemeral from cache
            h.ephemeral = ephemerals.get(&h.event_id).cloned();
            executions.push(h);
        }

        if !executions.is_empty() {
            did_work = true;
            // Cancellation check (batch)
            // Hydrate aggregates
            // Execute reactors in parallel
            // For each result:
            //   log.append(emitted_events)  ← FIRST
            //   queue.resolve(Complete/DLQ) ← THEN
        }

        // ── Phase 3: Check if settled ──
        if !did_work {
            match self.queue.earliest_pending_at().await? {
                Some(t) if t > now => { sleep(t - now).await; continue; }
                _ => break,
            }
        }
    }
    Ok(())
}
```

##### 3c. Rewrite `process_event`

Replaces `execute_event`. Takes `&PersistedEvent`, returns `IntentCommit`.

Responsibilities:
1. Hydrate cold aggregates (`log.load_stream` + `log.load_snapshot`)
2. Apply event to aggregator state
3. Auto-snapshot if configured (`log.save_snapshot`)
4. Match reactors, run projections
5. Build `IntentCommit` with checkpoint = event.position

##### 3d. Rewrite `build_new_event`

Replaces `build_queued_events`. Takes `EmittedEvent` + parent `QueuedReactor`,
returns `NewEvent` with routing metadata in `metadata` map:

- `_hops`: parent hops + 1
- `_batch_id`, `_batch_index`, `_batch_size`: from emitted event
- `_reactor_id`: reactor that produced this event
- Plus any engine-level metadata (`event_metadata` from `.with_event_metadata()`)

Deterministic event ID generation (UUID v5) unchanged.

##### 3e. Rewrite dispatch

```rust
pub async fn dispatch(&self, event: impl Event) -> Result<()> {
    let new_event = self.build_root_event(event);
    self.log.append(new_event).await?;
    // settle picks it up via load_from(checkpoint)
}
```

`build_root_event`: aggregate matching, metadata, deterministic ID, ephemeral.

##### 3f. Update supporting code

**`job_executor.rs`:**
- Field: `store: Arc<dyn Store>` → `queue: Arc<dyn ReactorQueue>`
- `execute_event` removed (replaced by `process_event` on Engine)
- `execute_handler` unchanged (uses `queue.load_journal` for replay)
- Passes `queue.clone()` to `Context::with_journal`

**`process.rs`:**
- Field: `store: Option<Arc<dyn Store>>` → `queue: Option<Arc<dyn ReactorQueue>>`
- `cancel()` → `queue.cancel()`
- `status()` → `queue.status()`

**`reactor/context.rs`:**
- `JournalState.store: Arc<dyn Store>` → `queue: Arc<dyn ReactorQueue>`
- `with_journal` takes `Arc<dyn ReactorQueue>`

**`event_store.rs`:**
- `persist_event` takes `&dyn EventLog` instead of `&dyn Store`
- `save_snapshot` takes `&dyn EventLog` instead of `&dyn Store`

##### 3g. Remove old code

- Delete `src/store.rs`
- Remove from `types.rs`: `QueuedEvent`, `EventCommit`, `EventOutcome`
- Remove `events_to_publish` from `HandlerCompletion` and `HandlerDlq`
- Remove `pending_events` from `QueueStatus`
- Remove old `impl Store for MemoryStore`
- Remove event buffer from MemoryStore (`events: Arc<DashMap<...>>`, `publish`, `poll_next`, `complete_event`)

##### 3h. Update `lib.rs` re-exports

Remove: `Store`, `QueuedEvent`, `EventCommit`, `EventOutcome`
Add: `EventLog`, `ReactorQueue`, `IntentCommit`, `EventPark`
Keep: all other existing re-exports

##### 3i. Update all tests

**`tests/engine_integration.rs`:**
- `Engine::new(deps)` → `Engine::in_memory(deps)` (bulk find-replace)
- `Engine::new(deps).with_store(store)` → `Engine::new(deps, store.clone(), store)`
- `store.global_log()` still works (MemoryStore helper, not on trait)
- Remove any references to `QueuedEvent`, `EventCommit`, `EventOutcome`

**`src/reactor/context.rs` tests:**
- All `Arc<dyn Store>` → `Arc<dyn ReactorQueue>`
- `MemoryStore::new()` still works (implements ReactorQueue)

**`src/event_store.rs` tests:**
- `MemoryStore::with_persistence()` → `MemoryStore::new()`
- `store.append_event(...)` → `store.append(...)` (EventLog method name)
- `store.load_global_from(...)` → `store.load_from(...)` (EventLog method name)

**Checkpoint:** `cargo test` passes. Old Store trait gone. New traits in use everywhere.

---

#### Phase 4: Polish

- Verify all doc comments reference correct trait names
- Update CLAUDE.md if it references Store trait
- Bump version (breaking change → minor version bump pre-1.0)

## Acceptance Criteria

### Functional Requirements

- [x] `EventLog` trait defined with 5 methods (3 required, 2 optional)
- [x] `ReactorQueue` trait defined with 13 methods (8 required, 5 optional)
- [x] `MemoryStore` implements both `EventLog` and `ReactorQueue`
- [x] `Engine` holds `Arc<dyn EventLog>` + `Arc<dyn ReactorQueue>`
- [x] `Engine::in_memory(deps)` convenience constructor works
- [x] Settle loop uses checkpoint-based reading from EventLog
- [x] Reactor results: engine appends emitted events to log, then resolves
- [x] Routing metadata (`_hops`, `_batch_id`, etc.) round-trips through metadata map
- [x] Ephemeral typed events preserved through MemoryStore path
- [x] `IntentCommit::park()` and `::skip()` advance checkpoint
- [x] All existing integration tests pass (updated for new API)
- [ ] `Store` trait, `QueuedEvent`, `EventCommit`, `EventOutcome` removed (deferred — Store kept for backwards compat)

### Non-Functional Requirements

- [x] No new `unsafe` code
- [x] No performance regression for MemoryStore path (ephemeral cache)
- [x] Zero-copy event path maintained (no extra clones)

### Quality Gates

- [x] `cargo test` passes (87 integration + 33 stress + 63 unit)
- [x] `cargo clippy` clean (no new warnings)
- [ ] `cargo doc` builds without warnings

## Rootsignal Adoption Checklist

The plan above covers causal-core. Rootsignal is the primary consumer and needs
these changes (NOT part of this PR, but tracked here for completeness):

### PostgresStore split

`PostgresStore` (838 lines) → `PostgresEventLog` + `PostgresReactorQueue`.

**PostgresEventLog** implements:
- `append` ← current `append_event` (add `metadata JSONB` column for routing keys)
- `load_from` ← current `load_global_from`
- `load_stream` ← current `load_stream`
- `load_snapshot` / `save_snapshot` ← unchanged

**PostgresReactorQueue** implements:
- `enqueue` ← current `commit_event` logic (insert intents + advance checkpoint)
- `checkpoint` ← new: `SELECT position FROM causal_checkpoints WHERE correlation_id = $1`
- `dequeue` ← current `poll_next_handler`
- `resolve` ← current `resolve_handler` (simplified: NO `events_to_publish`)
- `earliest_pending_at`, `reclaim_stale`, journaling, cancellation, descriptions ← unchanged

### Schema changes

- **Drop** `causal_events` table
- **Add** `causal_checkpoints` table: `(correlation_id UUID PK, position BIGINT)`
- **Add** `metadata JSONB` column to `events` table (for routing key round-trip)

### Engine builder functions

All 5 `build_*_engine` functions in `engine.rs` change signature:

```rust
// Before
pub fn build_engine(deps: ScoutEngineDeps, causal_store: Option<Arc<dyn causal::Store>>)

// After
pub fn build_engine(
    deps: ScoutEngineDeps,
    log: Option<Arc<dyn causal::EventLog>>,
    queue: Option<Arc<dyn causal::ReactorQueue>>,
)
```

When `log`/`queue` are `Some`, pass to `Engine::new(deps, log, queue)`.
When `None`, use `Engine::in_memory(deps)`.

### `make_store` factory

```rust
// Before
fn make_store(&self, run_id: Uuid) -> Option<Arc<dyn causal::Store>>

// After
fn make_store(&self, run_id: Uuid) -> (Arc<dyn causal::EventLog>, Arc<dyn causal::ReactorQueue>) {
    let log = Arc::new(PostgresEventLog::new(self.pg_pool.clone(), run_id));
    let queue = Arc::new(PostgresReactorQueue::new(self.pg_pool.clone(), run_id));
    (log, queue)
}
```

### Scout runner recovery flow

```rust
// Before (scout_runner.rs:274-325)
let store = PostgresStore::new(pool, run_id);
store.reclaim_stale().await?;
if store.has_pending_work().await? { ... }
let store_arc = Arc::new(store) as Arc<dyn Store>;
build_full_engine(deps, Some(store_arc))

// After
let queue = Arc::new(PostgresReactorQueue::new(pool.clone(), run_id));
queue.reclaim_stale().await?;
if queue.has_pending_work().await? { ... }  // custom method on PostgresReactorQueue
let log = Arc::new(PostgresEventLog::new(pool, run_id));
build_full_engine(deps, Some(log), Some(queue))
```

`has_pending_work()` stays as a custom method on `PostgresReactorQueue` (not on
the trait). It queries: pending reactors + events past checkpoint. Since both are
in the same Postgres database, it can join across tables.

### `build_infra_only_engine`

Creates both `PostgresEventLog` and `PostgresReactorQueue` scoped to `run_id`.

### `pending_events` in admin panel

Check if rootsignal's admin panel or flow diagram displays `pending_events` from
`QueueStatus`. If so, compute it engine-side:
`log.load_from(queue.checkpoint(), MAX).len()` — or add a rootsignal-specific
helper that queries `SELECT COUNT(*) FROM events WHERE seq > checkpoint`.

### Atomicity note

Current `complete_handler_inner` publishes emitted events atomically with reactor
completion (same Postgres transaction). New design: `log.append()` then
`queue.resolve()` are separate calls. For same-Postgres setup, this could be
wrapped in a Postgres transaction by having `PostgresEventLog` and
`PostgresReactorQueue` share a transaction handle — but the trait API doesn't
support that. The idempotency contract (dedup by event_id) covers crash safety.
Accept the behavioral change.

## Risk Analysis

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Large diff breaks something subtle | Medium | High | Phase 3 is one commit — `cargo test` is the gate |
| Rootsignal PostgresStore needs parallel update | Low | Medium | Rootsignal hasn't shipped; clean cut |
| Ephemeral cache adds memory pressure | Low | Low | Cache is per-settle-iteration, cleared each loop |
| `pending_events` removal breaks admin panel | Low | Medium | Engine can compute it on demand if needed |

## References

- Brainstorm: `docs/brainstorms/2026-03-08-kurrentdb-store-composition.md`
- Current Store trait: `crates/causal/src/store.rs`
- Current Engine: `crates/causal/src/engine.rs`
- Current MemoryStore: `crates/causal/src/memory_store.rs`
- Current types: `crates/causal/src/types.rs`
- KurrentDB roadmap: `docs/plans/2026-03-06-kurrentdb-integration-roadmap.md`
