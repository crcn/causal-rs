---
date: 2026-03-08
topic: kurrentdb-store-composition
---

# Splitting the Store: EventLog + HandlerQueue

## The Core Idea

The monolithic `Store` trait does three jobs:

1. **Event buffer** — `publish` / `poll_next` / `complete_event` (transient queue)
2. **Event log** — `append_event` / `load_stream` / `load_global_from` (durable history)
3. **Handler queue** — `poll_next_handler` / `resolve_handler` (work distribution)

Job 1 is redundant. Every event passes through the buffer AND the log — two
copies, two tables, for every event. The buffer exists because the engine needs
something to poll. But with a checkpoint, the log itself can be that something.

**Kill the event buffer. Two traits remain: EventLog and HandlerQueue.**

## The Settle Loop

Before:
```
publish(event) → causal_events          // buffer copy
append_event(event) → events            // durable copy
poll_next() → claim from causal_events
complete_event() → ack buffer + create handler intents
poll_next_handler() → claim handler
resolve_handler() → publish emitted events to causal_events  // buffer copy again
                   → append_event to events                   // durable copy again
loop
```

After:
```
log.append(event)                                  // one copy
events = log.load_from(last_checkpoint)            // read what's new
queue.enqueue(intents, new_checkpoint)             // create work + advance cursor
handler = queue.dequeue()                          // claim work
execute handler
log.append(emitted_events)                         // one copy (log first)
queue.resolve(Complete)                            // mark done (resolve second)
loop until nothing new past checkpoint and queue empty
```

Half the writes. No transient queue table. One copy of every event.

## Two Traits

```rust
/// Where events live. Durable, ordered, appendable.
///
/// Postgres events table. KurrentDB streams. In-memory Vec.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Append event to the log. Returns position + optional stream version.
    ///
    /// Total idempotency contract: duplicate event_id returns the existing
    /// position without creating a duplicate entry.
    async fn append(&self, event: NewEvent) -> Result<AppendResult>;

    /// Load events after a position (settle loop, catch-up).
    ///
    /// Scope is implementation-defined: a per-correlation EventLog returns
    /// only events for that correlation. A global EventLog returns all.
    async fn load_from(
        &self,
        after_position: u64,
        limit: usize,
    ) -> Result<Vec<PersistedEvent>>;

    /// Load events for an aggregate stream (hydration).
    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_version: Option<u64>,
    ) -> Result<Vec<PersistedEvent>>;

    /// Snapshots (optional — cache, not source of truth).
    async fn load_snapshot(
        &self, _t: &str, _id: Uuid,
    ) -> Result<Option<Snapshot>> { Ok(None) }

    async fn save_snapshot(
        &self, _s: Snapshot,
    ) -> Result<()> { Ok(()) }
}

/// Where handler work items live. Relational, claimable, resolvable.
///
/// Postgres causal_effect_executions. In-memory BTreeMap.
#[async_trait]
pub trait HandlerQueue: Send + Sync {
    /// Create handler intents and advance the checkpoint atomically.
    async fn enqueue(&self, commit: IntentCommit) -> Result<()>;

    /// Last processed EventLog position.
    async fn checkpoint(&self) -> Result<u64>;

    /// Claim next ready handler (pending → running).
    async fn dequeue(&self) -> Result<Option<QueuedHandler>>;

    /// Earliest future-scheduled handler (for sleep-until).
    async fn earliest_pending_at(&self) -> Result<Option<DateTime<Utc>>>;

    /// Complete, retry, or dead-letter a handler.
    async fn resolve(&self, resolution: HandlerResolution) -> Result<()>;

    /// Reclaim handlers stuck in running state past timeout.
    async fn reclaim_stale(&self) -> Result<()> { Ok(()) }

    // ── Journaling (handler retry replay) ──
    async fn load_journal(&self, handler_id: &str, event_id: Uuid) -> Result<Vec<JournalEntry>>;
    async fn append_journal(&self, handler_id: &str, event_id: Uuid, seq: u32, value: Value) -> Result<()>;
    async fn clear_journal(&self, handler_id: &str, event_id: Uuid) -> Result<()>;

    // ── Coordination ──
    async fn cancel(&self, correlation_id: Uuid) -> Result<()> { Ok(()) }
    async fn is_cancelled(&self, correlation_id: Uuid) -> Result<bool> { Ok(false) }
    async fn status(&self, correlation_id: Uuid) -> Result<QueueStatus> { Ok(QueueStatus::default()) }
    async fn set_descriptions(&self, correlation_id: Uuid, d: HashMap<String, Value>) -> Result<()> { Ok(()) }
    async fn get_descriptions(&self, correlation_id: Uuid) -> Result<HashMap<String, Value>> { Ok(HashMap::new()) }
}
```

### Trait design notes

**`load_from` vs `load_global_from`**: Renamed to `load_from` because scope is
implementation-defined. A `PostgresEventLog::new(pool, correlation_id)` filters
by correlation. A `KurrentDbEventLog` subscribing to `$all` returns everything.
The settle loop doesn't care — it just asks "what's new past my checkpoint?"

**No `subscribe_from`**: Push-based subscriptions are not needed for single-process
settle. The settle loop polls `load_from` on each iteration. Add `subscribe_from`
later for multi-node sync (Phase 3 of the roadmap) as an optional trait extension.

## Pressure-Tested Issues & Resolutions

### Issue 1: Routing metadata (hops, batch_id) has no home

`QueuedEvent` carries `hops`, `batch_id`, `batch_index`, `batch_size` — routing
metadata the engine needs. `PersistedEvent` doesn't have these fields.

**Resolution: Store in `NewEvent.metadata`.**

When a handler emits events, the engine builds `NewEvent` with routing metadata
in the metadata map:

```rust
fn build_new_event(&self, emitted: EmittedEvent, parent: &QueuedHandler) -> NewEvent {
    let mut metadata = self.event_metadata.clone();
    metadata.insert("hops", json!(parent.hops + 1));
    if let Some(bid) = emitted.batch_id {
        metadata.insert("batch_id", json!(bid));
        metadata.insert("batch_index", json!(emitted.batch_index));
        metadata.insert("batch_size", json!(emitted.batch_size));
    }
    // ...
}
```

When the settle loop loads events, it reads routing metadata back:

```rust
let hops = event.metadata.get("hops").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
if hops >= max_hops {
    // DLQ: infinite loop detected
}
```

This is how KurrentDB naturally works — metadata is a separate JSON field on each
event, distinct from the payload. Slightly impure (routing concerns in the log),
but pragmatic and aligned with KurrentDB's event model.

### Issue 2: Event-level failure creates an infinite loop

If processing an event fails (Phase 1), the checkpoint doesn't advance.
`load_from(checkpoint)` returns the same event. It fails again. Forever.

**Resolution: In-memory retry counter, following KurrentDB's subscription model.**

KurrentDB persistent subscriptions track `retryCount` per event per subscription.
After `maxRetryCount` (default 10), the event is automatically **parked** (DLQ'd)
and the subscription advances. Retry state lives in the subscription, not in the
event. The event in the log is immutable.

Our equivalent:

```rust
pub async fn settle(&self) -> Result<()> {
    // Retry state lives in the settle loop, not in the log or queue.
    // Reset on process restart — gives permanent errors fresh attempts,
    // transient errors will have resolved.
    let mut event_attempts: HashMap<u64, u32> = HashMap::new();

    loop {
        // ── Phase 1: Process new events ──
        let checkpoint = self.queue.checkpoint().await?;
        let events = self.log.load_from(checkpoint, BATCH_SIZE).await?;

        for event in events {
            let attempts = event_attempts.entry(event.position).or_insert(0);
            *attempts += 1;

            if *attempts > self.max_event_attempts {
                // Park: DLQ + advance checkpoint past it
                self.queue.enqueue(IntentCommit::park(
                    &event,
                    format!("event processing failed after {} attempts", attempts),
                )).await?;
                event_attempts.remove(&event.position);
                continue;
            }

            match self.process_event(&event).await {
                Ok(commit) => {
                    self.queue.enqueue(commit).await?;
                    event_attempts.remove(&event.position);
                }
                Err(e) => {
                    // Don't advance checkpoint. Next iteration retries.
                    tracing::warn!(
                        position = event.position,
                        attempt = *attempts,
                        "Event processing failed: {}", e
                    );
                    break; // retry from this position
                }
            }
        }
        // ...
    }
}
```

**Why in-memory is fine:** If the process crashes, it restarts from the last
checkpoint and re-encounters the event with a fresh counter. Permanent errors
(corrupt payload, missing aggregator) exhaust retries again quickly. Transient
errors (DB hiccup) will have resolved by the time the process restarts.

This matches KurrentDB's model: `retryCount` is per subscription lifetime,
not persisted globally. The subscription is the authority on processing state.

### Issue 3: `load_global_from` scoping

Rootsignal's `PostgresStore` is per-correlation, but `load_global_from` has no
correlation filter — it returns events from all correlations.

**Resolution: Rename to `load_from`, scope is implementation-defined.**

`PostgresEventLog::new(pool, correlation_id)` adds `AND correlation_id = $1`
to its queries. `KurrentDbEventLog` subscribing to a per-correlation stream
returns only that correlation's events. The trait doesn't prescribe scope.

The settle loop just calls `log.load_from(checkpoint, limit)` — it gets back
whatever events it should process, scoped by the implementation.

### Issue 4: Ephemeral typed events lost

Today, `emit()` creates an `Arc<dyn Any>` that carries the original typed
event through the dispatch cycle without JSON round-tripping.

**Resolution: Add optional `ephemeral` field to `PersistedEvent`.**

```rust
pub struct PersistedEvent {
    // ... existing fields ...
    /// Original typed event, available only during live dispatch in-process.
    /// None on load from durable store.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
}
```

`MemoryStore`'s EventLog stashes the ephemeral alongside the event and returns
it on `load_from`. Postgres/KurrentDB always return `None`. Handlers fall back
to JSON deserialization when ephemeral is absent (they already do this today for
replayed events).

### Issue 5: Aggregate metadata must be set before appending

Today, `persist_and_hydrate` determines `aggregate_type`/`aggregate_id` via
aggregator matching, then appends the event with that metadata. In the new
design, events are appended at emit time — before processing.

**Resolution: Match aggregators before appending.**

Aggregate matching is cheap — just `extract_id_from_json(&payload)`. No IO,
no hydration. The engine has all aggregator registrations at all times.

```rust
// Before appending (both at dispatch and handler emit time):
fn build_new_event(&self, emitted: EmittedEvent, ...) -> NewEvent {
    let (agg_type, agg_id) = self.aggregators
        .find_by_event_type(&emitted.event_type)
        .iter()
        .find_map(|agg| {
            agg.extract_id_from_json(&emitted.payload)
                .map(|id| (Some(agg.aggregate_type.clone()), Some(id)))
        })
        .unwrap_or((None, None));

    NewEvent {
        aggregate_type: agg_type,
        aggregate_id: agg_id,
        // ...
    }
}
```

**Matching vs hydration — separate concerns:**
- **Matching** = "which aggregate does this event belong to?" (cheap, from payload)
- **Hydration** = "load that aggregate's state" (expensive, from store)

Matching moves earlier (before append). Hydration stays where it is (during
settle, before handler execution).

**Bonus simplification:** Today's `persist_and_hydrate` carefully hydrates
BEFORE appending to avoid double-apply during `load_stream`. In the new design,
the event is always in the log when hydration happens. `load_stream(type, id,
Some(current_version))` with the `after_version` filter naturally skips already-
applied events. No special ordering dance needed.

### Issue 6: QueueStatus.pending_events semantics

With no event buffer, "pending events" becomes "events past checkpoint."

**Resolution:** Compute as `SELECT COUNT(*) FROM events WHERE correlation_id = $1
AND seq > (SELECT position FROM causal_checkpoints WHERE correlation_id = $1)`.

For rootsignal's flow diagram this is arguably better — it shows how far behind
processing is, not how many items sit in a transient queue.

## What Disappears

| Gone | Why |
|---|---|
| `Store` trait | Replaced by `EventLog` + `HandlerQueue` |
| `publish()` | No event buffer |
| `poll_next()` | No event buffer |
| `complete_event()` | Replaced by `enqueue()` |
| `EventOutcome` | No event rejection at buffer level |
| `EventCommit` | Replaced by `IntentCommit` |
| `QueuedEvent` | No event buffer (engine works with `PersistedEvent`) |
| `QueuedEvent.id` (row_id) | No buffer rows to ack |
| `HandlerCompletion.events_to_publish` | Engine appends to log directly |
| `HandlerDlq.events_to_publish` | Engine appends to log directly |
| `causal_events` table | No event buffer |

## New Types

```rust
/// Atomic intent creation payload.
pub struct IntentCommit {
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    pub intents: Vec<HandlerIntent>,
    pub projection_failures: Vec<ProjectionFailure>,
    pub handler_descriptions: HashMap<String, serde_json::Value>,
    /// Advance checkpoint to this EventLog position.
    pub checkpoint: u64,
    /// Park this event in DLQ (exceeded hops, exceeded retry, etc.)
    pub park: Option<EventPark>,
}

/// Reason for parking (DLQ-ing) an event during processing.
pub struct EventPark {
    pub reason: String,
}

/// Handler completion (no events_to_publish).
pub struct HandlerCompletion {
    pub event_id: Uuid,
    pub handler_id: String,
    pub result: serde_json::Value,
    pub log_entries: Vec<LogEntry>,
}

/// Handler DLQ (no events_to_publish — engine appends terminal events to log).
pub struct HandlerDlq {
    pub event_id: Uuid,
    pub handler_id: String,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
    pub log_entries: Vec<LogEntry>,
}
```

## Engine

```rust
pub struct Engine<D: Send + Sync + 'static> {
    log: Arc<dyn EventLog>,
    queue: Arc<dyn HandlerQueue>,
    deps: Arc<D>,
    handlers: Arc<HandlerRegistry<D>>,
    aggregators: Arc<AggregatorRegistry>,
    // ...
}
```

### Construction

```rust
// In-memory (tests, examples)
let store = MemoryStore::new();
let engine = Engine::new(deps, store.clone(), store);

// Postgres (rootsignal today)
let log = PostgresEventLog::new(pool.clone(), correlation_id);
let queue = PostgresHandlerQueue::new(pool, correlation_id);
let engine = Engine::new(deps, log, queue);

// KurrentDB (rootsignal future)
let log = KurrentDbEventLog::new(client, correlation_id);
let queue = PostgresHandlerQueue::new(pool, correlation_id);
let engine = Engine::new(deps, log, queue);
```

### Settle

```rust
pub async fn settle(&self) -> Result<()> {
    let mut event_attempts: HashMap<u64, u32> = HashMap::new();

    loop {
        let mut did_work = false;

        self.queue.reclaim_stale().await?;

        // ── Phase 1: Process new events from the log ──
        let checkpoint = self.queue.checkpoint().await?;
        let events = self.log.load_from(checkpoint, BATCH_SIZE).await?;
        for event in events {
            did_work = true;

            // Cancellation check
            if self.queue.is_cancelled(event.correlation_id).await? {
                self.queue.enqueue(IntentCommit::skip(&event)).await?;
                continue;
            }

            // Hop check (infinite loop detection)
            let hops = event.metadata.get("hops")
                .and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            if hops >= self.max_hops {
                self.queue.enqueue(IntentCommit::park(
                    &event, "exceeded max hops — infinite loop detected",
                )).await?;
                continue;
            }

            // Event-level retry tracking (KurrentDB subscription model)
            let attempts = event_attempts.entry(event.position).or_insert(0);
            *attempts += 1;
            if *attempts > self.max_event_attempts {
                self.queue.enqueue(IntentCommit::park(
                    &event,
                    format!("event processing failed after {} attempts", attempts),
                )).await?;
                event_attempts.remove(&event.position);
                continue;
            }

            // Process event: hydrate, apply aggregators, determine handlers
            match self.process_event(&event).await {
                Ok(commit) => {
                    self.queue.enqueue(commit).await?;
                    event_attempts.remove(&event.position);
                }
                Err(e) => {
                    tracing::warn!(
                        position = event.position, attempt = *attempts,
                        "Event processing failed: {}", e
                    );
                    break; // retry from this position next iteration
                }
            }
        }

        // ── Phase 2: Execute handlers ──
        let mut executions = vec![];
        while let Some(h) = self.queue.dequeue().await? {
            executions.push(h);
        }
        if !executions.is_empty() {
            did_work = true;
            self.hydrate_for_handlers(&executions).await?;

            let results = futures::future::join_all(
                executions.iter().map(|e| self.execute_handler(e))
            ).await;

            for (result, execution) in results.into_iter().zip(executions) {
                match result {
                    Ok(r) if r.status == Success => {
                        // Append emitted events to log FIRST (durable)
                        for emitted in &r.emitted_events {
                            let new_event = self.build_new_event(emitted, &execution);
                            self.log.append(new_event).await?;
                        }
                        // THEN mark handler complete
                        self.queue.resolve(Complete(HandlerCompletion {
                            event_id: execution.event_id,
                            handler_id: execution.handler_id,
                            result: r.result,
                            log_entries: r.log_entries,
                        })).await?;
                    }
                    Ok(r) if r.status == Failed { .. } | Timeout => {
                        // Append terminal events (on_failure/on_dlq) to log first
                        for emitted in &r.emitted_events {
                            let new_event = self.build_new_event(emitted, &execution);
                            self.log.append(new_event).await?;
                        }
                        self.queue.resolve(DeadLetter(HandlerDlq { .. })).await?;
                    }
                    // retry, error...
                }
            }
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

### Dispatch

```rust
pub async fn dispatch(&self, event: impl Event) -> Result<()> {
    let new_event = self.build_root_event(event); // includes aggregate matching
    self.log.append(new_event).await?;
    // settle picks it up via load_from(checkpoint)
}
```

## Atomicity

### Log before resolve

When a handler emits events:
```
1. log.append(emitted_events)     ← must succeed (durable)
2. queue.resolve(Complete)        ← marks handler done
```

Crash after 1, before 2: handler re-executes, re-appends (deduped by event_id),
resolves. Safe. Total idempotency contract on `append` guarantees this.

### Enqueue is atomic

`queue.enqueue(commit)` atomically: inserts handler intents + advances
checkpoint. If it fails, checkpoint stays at old position, engine reprocesses
the event on next loop. Idempotent.

### Event-level retry

In-memory counter, following KurrentDB's subscription model. Reset on process
restart. Permanent errors re-exhaust quickly (they're bugs). Transient errors
resolve between restarts.

## Rootsignal Adoption

Rootsignal hasn't shipped. Clean cut, no migration.

| Component | Change |
|---|---|
| `PostgresStore` | Split into `PostgresEventLog` (events table) + `PostgresHandlerQueue` (causal_effect_executions, journals, descriptions, cancellation) |
| `causal_events` table | **Drop.** |
| `causal_checkpoints` table | **New.** `(correlation_id, position)` |
| `event_broadcast.rs` | No change — pg_notify still fires from events table INSERT |
| `event_cache.rs` | No change |
| `scout_run.rs` queries | No change |
| `investigate_tools.rs` | No change |
| Engine init | `Engine::new(deps, pg_log, pg_queue)` |

### Net

- Drop `causal_events` table (~50% fewer event writes)
- One copy of every event instead of two
- `PostgresStore` (838 lines) → two focused structs
- Ready to swap `PostgresEventLog` → `KurrentDbEventLog` with zero other changes

## MemoryStore

Single struct implements both traits:

```rust
impl EventLog for MemoryStore { ... }
impl HandlerQueue for MemoryStore { ... }
```

## KurrentDB Stream Strategy

### The Problem

KurrentDB requires every event to go to a **named stream**. There is no
"append to the global log" — `$all` is a derived view across all streams.
Causal doesn't have an explicit stream name concept; it has
`(aggregate_type, aggregate_id)` fields on `NewEvent`.

Three issues:
1. Non-aggregate events (`aggregate_type: None`) have no stream target
2. `load_from` scope (per-correlation in rootsignal) doesn't map to a
   single KurrentDB stream without a strategy
3. An event lives in exactly ONE stream — if it goes to the aggregate
   stream, it's not in a correlation stream (unless via link events from
   system projections, which have eventual consistency lag)

### Two Stream Strategies

**Strategy A: Correlation-Primary** (recommended for rootsignal)

All events go to `correlation-{correlationId}`. One stream per workflow run.

```
append(event):
  stream = "correlation-{event.correlation_id}"
  client.append_to_stream(stream, ExpectedRevision::Exact(last), event)
  // Set $correlationId, $causationId in KurrentDB metadata

load_from(position, limit):
  client.read_stream("correlation-{self.correlation_id}", after: position)

load_stream(agg_type, agg_id, after_version):
  // Filter within the correlation stream (client-side)
  events = client.read_stream("correlation-{self.correlation_id}")
  events.filter(|e| e.agg_type == type && e.agg_id == id && e.version > v)
```

Pressure-tested:

| Concern | Assessment |
|---------|------------|
| **Aggregate hydration perf** | Rootsignal's PipelineState is a singleton — all events apply. SignalLifecycle/ConcernLifecycle filter a few events from hundreds. Negligible. |
| **Optimistic concurrency** | Stream revision provides natural ordering. `ExpectedRevision::Exact` ensures no concurrent writes to same correlation. STRONGER than current Postgres model. |
| **Idempotency** | With `ExpectedRevision::Exact`, KurrentDB checks event IDs at expected revision. Crash-retry appends same IDs at same revision → deduped. Total idempotency WITHOUT external dedup index. |
| **Cross-correlation queries** | Not needed during settle. For admin/debug: use `$all` with server-side filtering or `$et-{EventType}` projections. Read-model concerns. |
| **Non-aggregate events** | All events go to the correlation stream regardless. No orphans. |
| **System projection dependency** | None. No `$bc-` or `$ce-` needed for the hot path. System projections remain available for observability. |
| **`load_stream` scaling cliff** | Hydration cost grows with correlation length, not aggregate stream length. Fine for rootsignal's shape (hundreds of events per correlation). If a future use case has 10k+ events per correlation with many small aggregates, revisit — either add aggregate sub-streams or switch to Strategy B. Comment in code so whoever hits it first understands the tradeoff. |

**Strategy B: Aggregate-Primary** (classic event sourcing)

Aggregate events go to `{AggregateType}-{AggregateId}`. Non-aggregate events
go to `unscoped-{correlationId}`. `load_from` reads from `$bc-{correlationId}`
system projection.

**Rejected for rootsignal** because:
- `$bc-` system projection runs on leader only (CPU/IO cost)
- Eventual consistency lag between append and `load_from` — the settle loop
  could miss events it just wrote. Correctness risk in the hot path.
- Non-aggregate events need a catch-all stream convention

May be appropriate for a future use case with millions of aggregate instances
where per-aggregate stream reads are critical for hydration performance.

### KurrentDB Metadata Mapping

KurrentDB events have separate `data` and `metadata` byte arrays. Map:

| Causal field | KurrentDB location |
|---|---|
| `payload` | Event `data` (JSON) |
| `event_type` | Event `event_type` string |
| `event_id` | Event `event_id` UUID |
| `correlation_id` | Metadata `$correlationId` |
| `parent_id` | Metadata `$causationId` |
| `aggregate_type` | Metadata `_aggregate_type` |
| `aggregate_id` | Metadata `_aggregate_id` |
| `_hops`, `_batch_*`, `_handler_id` | Metadata (routing, underscore-prefixed) |
| Domain keys (`run_id`, etc.) | Metadata (unprefixed) |

Setting `$correlationId` and `$causationId` enables KurrentDB's built-in
system projections (`$bc-`, `$et-`) for debugging/observability — even when
using Strategy A for the hot path.

### Why the Trait Doesn't Need Stream Names

The `EventLog` trait stays clean:

```rust
async fn append(&self, event: NewEvent) -> Result<AppendResult>;
async fn load_from(&self, after_position: u64, limit: usize) -> Result<Vec<PersistedEvent>>;
async fn load_stream(&self, agg_type: &str, agg_id: Uuid, after: Option<u64>) -> Result<Vec<PersistedEvent>>;
```

Stream naming is an **implementation detail**:
- `PostgresEventLog`: no streams — single `events` table with WHERE clauses
- `KurrentDbEventLog`: derives stream name from `correlation_id` (Strategy A)
  or `aggregate_type`/`aggregate_id` (Strategy B)
- `MemoryStore`: no streams — `Vec<PersistedEvent>` with filters

The trait operates on logical concepts (events, positions, aggregate
identities). The KurrentDB impl maps these to physical streams. If a future
use case needs explicit stream targeting, add an optional `stream` field to
`NewEvent` — but YAGNI for now.

## KurrentDB Path

Swap one line:

```rust
// Before
let log = PostgresEventLog::new(pool.clone(), cid);
// After
let log = KurrentDbEventLog::new(client, cid);
```

HandlerQueue stays Postgres. Everything else stays the same.

### Postgres Events Table — Full Mirror, Not Read Model

When KurrentDB is the primary EventLog, the Postgres `events` table becomes
a **full mirror** — not a read model with retention. The distinction matters:

- **Read model with retention** implies data can be dropped after N days.
  Wrong for rootsignal. Epistemic integrity, audit trail, provenance queries,
  and the admin panel's event timeline all depend on complete event history.
  "30-day retention" would silently destroy the foundation rootsignal's
  truth-claims are built on.

- **Full mirror** means every event in KurrentDB is replicated to Postgres
  via a catch-up subscriber. KurrentDB is the system of record; Postgres is
  an operational replica that supports SQL queries, pg_notify, and the
  existing event cache. If Postgres loses data, it can be rebuilt from
  KurrentDB. If KurrentDB loses data... that's what KurrentDB's cluster
  replication is for.

The catch-up subscriber is a separate long-running process:
```
subscribe_to_all($all, from: last_postgres_seq)
  for each event:
    INSERT INTO events (...) ON CONFLICT (id) DO NOTHING
    pg_notify('events', seq)
```

Existing queries, pg_notify, event cache — all unchanged. The only
difference is WHERE events originate (KurrentDB instead of direct INSERT).

### KurrentDB Idempotency — No External Dedup Index Needed

With Strategy A (correlation-primary), the Engine tracks the stream revision
after each append. On retry after crash:

```
1. Engine reads correlation stream → gets current revision (e.g., 42)
2. Engine appends event with ExpectedRevision::Exact(42), EventId = deterministic UUID
3. KurrentDB checks: events at revision 42+ have this EventId? Yes → returns success (dedup)
4. No external dedup index required
```

This only works because:
- All events for a correlation go to ONE stream (Strategy A)
- Event IDs are deterministic (UUID v5 from parent + handler + index)
- The engine always knows the expected revision (from the last successful append)

Strategy B would need an external dedup index because events go to MULTIPLE
streams and `ExpectedRevision::Any` doesn't guarantee idempotency.

### Cold-Start Revision Initialization

On cold start or restart, the engine does NOT know the current stream
revision. It must read the tail of `correlation-{cid}` before the first
append:

```rust
impl KurrentDbEventLog {
    pub async fn new(client: Client, correlation_id: Uuid) -> Result<Self> {
        let stream = format!("correlation-{}", correlation_id);

        // Read last event to get current stream revision.
        // ReadStreamOptions::backwards().max_count(1) is O(1).
        let revision = match client
            .read_stream(stream, &ReadStreamOptions::default().backwards().max_count(1))
            .await
        {
            Ok(mut events) => match events.next().await? {
                Some(event) => event.event.revision,
                None => 0, // empty stream
            },
            Err(ReadStreamError::NotFound) => 0, // stream doesn't exist yet
        };

        Ok(Self { client, correlation_id, stream, revision })
    }
}
```

Miss this and the first append after restart uses a stale/zero revision →
`WrongExpectedVersion` error. The init read is O(1) (backwards, limit 1).

## Crate Structure

```
crates/
  causal/                    # EventLog + HandlerQueue traits, MemoryStore, Engine
  causal_core_macros/        # existing
  causal_utils/              # existing
  causal-kurrentdb/          # KurrentDbEventLog (future)
```

PostgresEventLog and PostgresHandlerQueue live in rootsignal until a second
app needs them.

## Notes from Review

**Poison pill in long-running processes:** The in-memory retry counter could
theoretically park a healthy event if transient errors are spread across days in
a long-running consumer. Not a concern for rootsignal (settle loops are per-run,
finite). For a future long-running subscription consumer, add a staleness reset:
`(attempts, last_seen_at)` — reset attempts if last_seen_at is older than a
threshold. YAGNI for now.

**Phase 1/2 interleaving:** Processing all events before any handlers means
emitted high-priority events wait for the current batch to finish. This is the
same behavior as today (current engine drains ALL events, then ALL handlers, per
iteration). Not a regression. If latency matters later, process events in smaller
chunks.

**Consistency gap:** The log may be "ahead" of the checkpoint (events appended
but not yet processed into handler intents). This already exists today —
`append_event` fires `pg_notify` before `complete_event` creates intents. Admin
panel already sees events before their handlers exist. No behavior change.

**Metadata discipline:** Two non-overlapping key sets share the metadata map.
Causal routing keys use underscore prefix: `_hops` (i32), `_batch_id` (UUID),
`_batch_index` (i32), `_batch_size` (i32), `_handler_id` (String — handler
that produced this event). Domain keys (rootsignal: `run_id`, `schema_v`,
`handler_id`) are unprefixed. Routing keys MUST round-trip through the store —
the settle loop reads `_hops` from `PersistedEvent.metadata` via `load_from`
for infinite loop detection. If routing keys are lost on round-trip, the hops
check silently defaults to 0 and infinite loops go undetected.

**Metadata round-trip in Postgres:** Rootsignal's current `into_persisted`
reconstructs metadata from only 3 promoted columns (`run_id`, `schema_v`,
`handler_id`). Routing keys (`_hops`, `_batch_id`, etc.) are silently dropped.
Fix: add a `metadata JSONB` column to the Postgres events table. Store the
full metadata map in this column. Promoted columns remain for indexed queries.
KurrentDB and MemoryStore don't have this problem — KurrentDB stores metadata
as a separate byte array per event, MemoryStore keeps the full struct in memory.

If querying events by `_batch_id` from metadata, add a functional GIN index on
the Postgres events table:
```sql
CREATE INDEX idx_events_batch_id ON events ((metadata->>'_batch_id'))
    WHERE metadata->>'_batch_id' IS NOT NULL;
```
Or promote `_batch_id` to a top-level column if query patterns warrant it.

### Issue 7: Ephemeral typed events through the queue

Current `EventCommit` carries `ephemeral: Option<Arc<dyn Any + Send + Sync>>` so
the store can pass the typed event through to `QueuedHandler` without JSON
round-tripping. The brainstorm's `IntentCommit` doesn't have this field.

Without it, even MemoryStore handlers would need JSON deserialization on every
handler execution — a performance regression for the common case.

**Resolution: Engine-side ephemeral cache, not on IntentCommit.**

The queue shouldn't know about ephemerals — they're an optimization for
in-process dispatch. Instead, the engine keeps a transient cache during settle:

```rust
// In settle(), populated during Phase 1:
let mut ephemerals: HashMap<Uuid, Arc<dyn Any + Send + Sync>> = HashMap::new();

for event in events {
    if let Some(eph) = &event.ephemeral {
        ephemerals.insert(event.event_id, eph.clone());
    }
    // ... process_event ...
}

// In Phase 2, inject into QueuedHandler after dequeue:
while let Some(mut h) = self.queue.dequeue().await? {
    h.ephemeral = ephemerals.get(&h.event_id).cloned();
    executions.push(h);
}
```

Clean separation: the queue is durable infrastructure, ephemerals are a
single-process optimization layered on top.

### Issue 8: Aggregator double-application on enqueue failure

If `process_event` applies an event to aggregator state and then
`queue.enqueue(commit)` fails, the next settle iteration re-reads the same
event via `load_from(checkpoint)` and calls `process_event` again — applying
the event to aggregator state a second time.

This is an existing issue in the current engine (same risk with
`complete_event` failure after `apply_to_aggregators`), not introduced by
this design. But worth documenting.

**Resolution: Version-guard in aggregator apply.**

Track the last-applied event position per aggregate. Skip events at or below
the last-applied position. Cheap (one comparison) and makes apply idempotent:

```rust
fn apply_to_aggregators(&self, event: &PersistedEvent) {
    for agg in self.aggregators.find_by_event_type(&event.event_type) {
        let key = format!("{}:{}", agg.aggregate_type, agg.id_or_nil(event));
        if agg.last_applied_position(&key) >= Some(event.position) {
            continue; // already applied
        }
        agg.apply(event);
        agg.set_last_applied_position(&key, event.position);
    }
}
```

Not blocking for the trait split — can be addressed as a separate fix that
benefits both old and new engine.

## Remaining Open Questions

1. **Snapshots** — belong on EventLog (where hydration happens). For KurrentDB,
   delegate to a Postgres-backed snapshot store. Snapshots are a cache.

2. **Multi-node subscription** — optional `subscribe_from()` on EventLog for
   push-based delivery. Add when cross-node sync is needed. Not blocking.

## Decisions

- **Drop monolithic `Store` trait** — two traits: `EventLog` + `HandlerQueue`
- **Kill the event buffer** — EventLog + checkpoint replaces `causal_events`
- **Correlation-primary streams for KurrentDB** — all events to `correlation-{cid}`,
  no system projection dependency in hot path, idempotency via `ExpectedRevision::Exact`
- **Stream names are implementation details** — trait operates on logical concepts,
  implementations map to physical streams. No `stream_name` field on `NewEvent`.
- **Routing metadata in event metadata** — hops, batch_id in `NewEvent.metadata`
- **In-memory event retry counter** — KurrentDB subscription model: retry state
  in the consumer, not the log. Park after N failures.
- **Aggregate matching before append** — cheap JSON extraction, no IO
- **`load_from` replaces `load_global_from`** — scope is implementation-defined
- **Log before resolve** — emitted events appended before handler marked complete
- **Adopt in rootsignal now** — no migration needed, clean cut
