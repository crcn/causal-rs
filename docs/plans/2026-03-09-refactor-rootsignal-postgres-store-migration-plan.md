---
title: "refactor: Migrate rootsignal PostgresStore to EventLog + ReactorQueue"
type: refactor
date: 2026-03-09
depends_on: docs/plans/2026-03-08-refactor-store-trait-split-plan.md
---

# Migrate rootsignal PostgresStore to EventLog + ReactorQueue

## Overview

Migrate rootsignal's `PostgresStore` from the removed monolithic `Store` trait to the
new `EventLog + ReactorQueue` split. Drop the transient `causal_events` queue table in
favor of checkpoint-based event processing from the permanent `events` table.

**Key insight**: The admin-app visualization is completely unaffected. It reads from
`events`, `causal_effect_executions`, `causal_reactor_descriptions`, and
`causal_reactor_logs` — none of which change.

## Problem Statement

After the causal-rs store trait split (PR #2), rootsignal's `PostgresStore` implements
the deleted `Store` trait. It won't compile against the new causal version.

Additionally, rootsignal's `causal_events` table is a transient work queue that
duplicates every event (once in the permanent `events` log, once in `causal_events`
for dispatch). The new checkpoint model eliminates this redundancy.

## What Changes

### Postgres Tables

**Dropped**: `causal_events` (transient queue table)

The `causal_events` table is only used for internal event dispatch:
- INSERT with `status = 'pending'` when events arrive
- UPDATE to `status = 'processing'` via `FOR UPDATE SKIP LOCKED`
- UPDATE to `status = 'done'` or `'rejected'` when processed
- `reclaim_stale` resets `'processing'` → `'pending'` for crash recovery

No admin-app query reads from this table. The causal flow visualization, event
browser, reactor outcomes, reactor descriptions, and reactor logs all read from
other tables.

**New table**: `causal_checkpoint`

```sql
CREATE TABLE causal_checkpoint (
    id TEXT PRIMARY KEY DEFAULT 'default',
    position BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO causal_checkpoint (id, position) VALUES ('default', 0);
```

Single row tracks how far the engine has processed in the `events` table. The engine
calls `EventLog::load_from(checkpoint, limit)` to discover new events, then advances
the checkpoint via `IntentCommit.checkpoint` when committing reactor intents.

**Unchanged tables** (all visualization data):

| Table | Purpose | Read by admin-app? |
|---|---|---|
| `events` | Permanent append-only event log | Yes — event browser, causal flow, realtime subscription |
| `causal_effect_executions` | Reactor lifecycle tracking | Yes — reactor outcomes, running/completed/error status |
| `causal_reactor_descriptions` | describe() block data | Yes — Block DSL widgets in flow nodes |
| `causal_reactor_logs` | Reactor log entries | Yes — logs pane |
| `causal_dead_letter_queue` | DLQ entries | Yes — error display |
| `causal_cancellations` | Correlation cancellation | No |
| `aggregate_snapshots` | Aggregate state snapshots | No |
| `causal_join_windows` | Batch join tracking | No |
| `causal_join_entries` | Batch join entries | No |

### Event Processing Flow

**Before** (double-write):
```
event arrives
  → INSERT into `events` (permanent log) + pg_notify('events', seq)
  → INSERT into `causal_events` (transient queue, status='pending')
  → engine polls `causal_events` via FOR UPDATE SKIP LOCKED
  → UPDATE status='processing'
  → process reactors
  → UPDATE status='done'
  → reactor emits new events → INSERT into `causal_events` again
```

**After** (checkpoint):
```
event arrives
  → INSERT into `events` (permanent log) + pg_notify('events', seq)
  → engine calls load_from(checkpoint) on `events` table
  → process reactors, create intents
  → enqueue(IntentCommit) advances checkpoint
  → reactor emits new events → INSERT into `events` + pg_notify
```

One write per event instead of two. The `events` table's `seq` column is the
ordering key. `pg_notify('events', seq)` wakes the engine to call `load_from`.

### Rust Trait Migration

**Before**: `impl Store for PostgresStore` (~838 lines, one big impl block)

**After**: Two impl blocks on the same struct:

```rust
#[async_trait]
impl EventLog for PostgresStore {
    async fn append(&self, event: NewEvent) -> Result<AppendResult> { ... }
    async fn load_from(&self, after_position: u64, limit: usize) -> Result<Vec<PersistedEvent>> { ... }
    async fn load_stream(&self, aggregate_type: &str, aggregate_id: Uuid, after_version: Option<u64>) -> Result<Vec<PersistedEvent>> { ... }
    async fn load_snapshot(&self, aggregate_type: &str, aggregate_id: Uuid) -> Result<Option<Snapshot>> { ... }
    async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()> { ... }
}

#[async_trait]
impl ReactorQueue for PostgresStore {
    async fn enqueue(&self, commit: IntentCommit) -> Result<()> { ... }
    async fn checkpoint(&self) -> Result<u64> { ... }
    async fn dequeue(&self) -> Result<Option<QueuedReactor>> { ... }
    async fn earliest_pending_at(&self) -> Result<Option<DateTime<Utc>>> { ... }
    async fn resolve(&self, resolution: HandlerResolution) -> Result<()> { ... }
    async fn cancel(&self, correlation_id: Uuid) -> Result<()> { ... }
    async fn is_cancelled(&self, correlation_id: Uuid) -> Result<bool> { ... }
    async fn status(&self, correlation_id: Uuid) -> Result<QueueStatus> { ... }
    async fn set_descriptions(&self, correlation_id: Uuid, descriptions: HashMap<String, serde_json::Value>) -> Result<()> { ... }
    async fn get_descriptions(&self, correlation_id: Uuid) -> Result<HashMap<String, serde_json::Value>> { ... }
    async fn load_journal(&self, reactor_id: &str, event_id: Uuid) -> Result<Vec<JournalEntry>> { ... }
    async fn append_journal(&self, reactor_id: &str, event_id: Uuid, seq: u32, value: serde_json::Value) -> Result<()> { ... }
    async fn clear_journal(&self, reactor_id: &str, event_id: Uuid) -> Result<()> { ... }
}
```

### Method Renames

| Old Store method | New trait | New method | Notes |
|---|---|---|---|
| `append_event(NewEvent)` | EventLog | `append(NewEvent)` | Same body |
| `load_global_from(pos, limit)` | EventLog | `load_from(pos, limit)` | Same body |
| `load_stream(type, id, after)` | EventLog | `load_stream(type, id, after)` | Same |
| `load_snapshot(type, id)` | EventLog | `load_snapshot(type, id)` | Same |
| `save_snapshot(snapshot)` | EventLog | `save_snapshot(snapshot)` | Same |
| `enqueue_handler_intents(...)` | ReactorQueue | `enqueue(IntentCommit)` | Destructure IntentCommit |
| — (new) | ReactorQueue | `checkpoint()` | SELECT from causal_checkpoint |
| `poll_next_handler()` | ReactorQueue | `dequeue()` | Same body |
| `earliest_pending_handler_at()` | ReactorQueue | `earliest_pending_at()` | Same body |
| `resolve_handler(resolution)` | ReactorQueue | `resolve(resolution)` | Remove events_to_publish handling |
| `cancel_correlation(id)` | ReactorQueue | `cancel(id)` | Same body |
| `is_cancelled(id)` | ReactorQueue | `is_cancelled(id)` | Same |
| `get_handler_descriptions(id)` | ReactorQueue | `get_descriptions(id)` | Same body |
| `set_handler_descriptions(id, d)` | ReactorQueue | `set_descriptions(id, d)` | Same body |

### Biggest Change: `enqueue(IntentCommit)`

The old `enqueue_handler_intents` took individual args. The new method takes a single struct:

```rust
pub struct IntentCommit {
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    pub intents: Vec<ReactorIntent>,
    pub handler_descriptions: HashMap<String, serde_json::Value>,
    pub projection_failures: Vec<ProjectionFailure>,
    pub park: Option<EventPark>,
    pub checkpoint: u64,
}
```

Implementation:

```rust
async fn enqueue(&self, commit: IntentCommit) -> Result<()> {
    let mut tx = self.pool.begin().await?;

    // 1. Insert reactor intents into causal_effect_executions (unchanged)
    for intent in &commit.intents {
        sqlx::query("INSERT INTO causal_effect_executions ...")
            .bind(commit.event_id)
            .bind(&intent.reactor_id)
            .bind(commit.correlation_id)
            // ... same columns as before
            .execute(&mut *tx)
            .await?;
    }

    // 2. Store reactor descriptions (unchanged)
    for (reactor_id, description) in &commit.handler_descriptions {
        sqlx::query("INSERT INTO causal_reactor_descriptions ... ON CONFLICT UPDATE ...")
            .bind(commit.correlation_id)
            .bind(reactor_id)
            .bind(description)
            .execute(&mut *tx)
            .await?;
    }

    // 3. Handle projection failures → DLQ (unchanged)
    for failure in &commit.projection_failures {
        sqlx::query("INSERT INTO causal_dead_letter_queue ...")
            .execute(&mut *tx)
            .await?;
    }

    // 4. Handle park → DLQ (unchanged)
    if let Some(park) = &commit.park {
        sqlx::query("INSERT INTO causal_dead_letter_queue ...")
            .execute(&mut *tx)
            .await?;
    }

    // 5. Advance checkpoint (NEW)
    sqlx::query(
        "UPDATE causal_checkpoint SET position = $1, updated_at = now() WHERE id = 'default'"
    )
    .bind(commit.checkpoint as i64)
    .execute(&mut *tx)
    .await?;

    // 6. NO INSERT into causal_events (REMOVED)

    tx.commit().await?;
    Ok(())
}
```

### `resolve()` Change: Remove `events_to_publish`

The old `resolve_handler(Complete)` would publish follow-on events into `causal_events`.
In the new model, the engine emits follow-on events through `EventLog::append` directly
(which writes to `events` and fires `pg_notify`). The `resolve` method just updates
reactor status and clears journal.

```rust
// REMOVE from resolve(Complete):
// - INSERT into causal_events for events_to_publish
// - The events_to_publish field no longer exists on HandlerCompletion
```

### Callsite Changes (5 files)

**`rootsignal-scout/src/core/engine.rs`** (5 engine builder functions):
```rust
// Before:
let store: Arc<dyn causal::Store> = Arc::new(PostgresStore::new(pool));
Engine::new(deps).with_store(store)

// After (with_store is now generic, no trait object needed):
let store = Arc::new(PostgresStore::new(pool));
Engine::new(deps).with_store(store)
```

**`rootsignal-scout/src/store/mod.rs`** (EngineFactory):
```rust
// Before:
pub fn store(&self) -> Arc<dyn causal::Store>

// After:
pub fn store(&self) -> Arc<PostgresStore>
```

**`rootsignal-scout/src/workflows/mod.rs`** (`make_store()`):
```rust
// Before:
fn make_store() -> Option<Arc<dyn causal::Store>>

// After:
fn make_store() -> Option<Arc<PostgresStore>>
```

**`rootsignal-api/src/scout_runner.rs`**:
```rust
// Before:
let store: Arc<dyn Store> = Arc::new(PostgresStore::new(pool));
store.reclaim_stale().await?;
store.has_pending_work().await?;

// After:
let store = Arc::new(PostgresStore::new(pool));
store.reclaim_stale().await?;  // Keep as inherent method on PostgresStore
store.has_pending_work().await?;  // Keep as inherent method
```

Note: `reclaim_stale()` and `has_pending_work()` are inherent methods on `PostgresStore`,
not trait methods. They continue to work. `has_pending_work` currently checks both
`causal_events` and `causal_effect_executions` — after migration it only checks
`causal_effect_executions` (reactor queue) and whether checkpoint < max(events.seq).

### Removed Methods

These `Store` trait methods have no equivalent in the new traits and should be deleted
from `PostgresStore`:

| Method | Reason for removal |
|---|---|
| `publish(events)` | Was for `causal_events` queue — replaced by checkpoint |
| `poll_next()` | Was for `causal_events` queue — replaced by `EventLog::load_from` |
| `complete_event(event_id)` | Was for `causal_events` status tracking |
| `reject_event(event_id, reason)` | Was for `causal_events` rejection |
| `reclaim_stale_events()` | No queue to reclaim from |
| `queue_status().pending_events` | No event queue to count |

Keep `reclaim_stale()` as an inherent method for reactor execution recovery (resetting
`'running'` → `'pending'` in `causal_effect_executions`).

## Admin-App Visualization Compatibility

### Verified Safe

Every admin-app feature was audited against the table changes:

| Feature | Data Source | Affected? |
|---|---|---|
| Event browser (paginated, searchable) | `events` table | No |
| Realtime event stream (WebSocket) | `events` + `pg_notify('events', seq)` | No |
| Causal tree (correlation_id drill-down) | `events` WHERE correlation_id | No |
| React Flow graph (DAG visualization) | `events` + `causal_effect_executions` + `causal_reactor_descriptions` | No |
| Currently running reactors (yellow pulse) | `causal_effect_executions` WHERE status='running' | No |
| Reactor outcomes (green/red/yellow borders) | `causal_effect_executions` GROUP BY reactor_id | No |
| describe() Block DSL widgets | `causal_reactor_descriptions` polled every 5s | No |
| Reactor logs | `causal_reactor_logs` | No |
| DLQ entries | `causal_dead_letter_queue` | No |

### Realtime Mechanism Unchanged

- `EventLog::append` still writes to `events` table and fires `pg_notify('events', seq)`
- `EventBroadcast` (rootsignal-api) still runs `PgListener` on channel `"events"`
- GraphQL subscription `events(lastSeq)` still does catch-up + broadcast
- Flow pane still appends live events matching `flowRunId`
- Reactor descriptions and outcomes still poll every 5 seconds

### `reactor_id` Column on `events` Table

The `events` table has a `reactor_id` column that records which reactor emitted each
event. This is set during `EventLog::append` (via `NewEvent.metadata` or a dedicated
field) and is used by the flow visualization to group events by emitting reactor.

This column is populated by the engine when persisting reactor output events — it's
independent of the queue mechanism and continues to work.

## SQL Migration

```sql
-- Migration: Drop causal_events queue, add checkpoint table

-- 1. Create checkpoint table
CREATE TABLE IF NOT EXISTS causal_checkpoint (
    id TEXT PRIMARY KEY DEFAULT 'default',
    position BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 2. Seed checkpoint from current causal_events high-water mark
-- This ensures no events are re-processed after migration
INSERT INTO causal_checkpoint (id, position)
SELECT 'default', COALESCE(
    (SELECT MAX(e.seq) FROM events e
     INNER JOIN causal_events se ON se.event_id = e.id
     WHERE se.status = 'done'),
    0
)
ON CONFLICT (id) DO NOTHING;

-- 3. Drop the queue table (after confirming migration works)
-- Run this in a follow-up migration after the code deploys successfully
-- DROP TABLE IF EXISTS causal_events;
```

The `DROP TABLE` is commented out intentionally — run it in a follow-up migration
after verifying the new checkpoint flow works in production.

## Deployment Strategy

### Phase 1: Code Migration (no schema changes)

1. Upgrade causal dependency in rootsignal
2. Split `impl Store` into `impl EventLog` + `impl ReactorQueue`
3. Keep `causal_events` INSERT in `enqueue()` temporarily (belt and suspenders)
4. Add `checkpoint()` reading from a new `causal_checkpoint` table
5. Replace `Arc<dyn Store>` with `Arc<PostgresStore>` at all callsites
6. Deploy, verify admin-app works identically

### Phase 2: Drop the Queue (schema change)

1. Remove `causal_events` INSERT from `enqueue()`
2. Remove `poll_next()`, `complete_event()`, `reject_event()` methods
3. Update `has_pending_work()` to check checkpoint vs max(events.seq)
4. Run the `DROP TABLE causal_events` migration
5. Deploy, verify

### Rollback Plan

If Phase 2 fails:
- Revert code to Phase 1 (still writes to `causal_events`)
- Re-create `causal_events` table from the migration backup
- No data loss — the `events` table is always the source of truth

## Acceptance Criteria

- [ ] `PostgresStore` compiles against new causal version
- [ ] `impl EventLog for PostgresStore` passes all EventLog contract tests
- [ ] `impl ReactorQueue for PostgresStore` passes all ReactorQueue contract tests
- [ ] Admin-app event browser works (paginated, searchable)
- [ ] Admin-app realtime events arrive via WebSocket
- [ ] Admin-app causal flow renders React Flow DAG correctly
- [ ] Admin-app reactor status shows running/completed/error states
- [ ] Admin-app describe() blocks render and update in realtime
- [ ] Admin-app reactor logs display correctly
- [ ] `causal_events` table no longer written to
- [ ] Checkpoint advances correctly through event processing
- [ ] `has_pending_work()` uses checkpoint comparison
- [ ] `reclaim_stale()` still recovers stuck reactor executions
- [ ] No double-write of events (single INSERT into `events` table)

## References

- Causal store trait split: PR #2 (`refactor/store-trait-split` branch)
- Parent plan: `docs/plans/2026-03-08-refactor-store-trait-split-plan.md`
- PostgresStore: `rootsignal-scout/src/core/postgres_store.rs`
- Admin-app flow viz: `admin-app/src/pages/events/panes/CausalFlowPane.tsx`
- Event broadcast: `rootsignal-api/src/event_broadcast.rs`
- GraphQL schema: `rootsignal-api/src/graphql/schema.rs`
