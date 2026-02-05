---
title: Queue-Backed Architecture for Seesaw
type: feat
date: 2026-02-05
status: draft
context: Single-developer project, pragmatic approach
---

# Queue-Backed Architecture for Seesaw

## Context

**Target User**: Single developer (no backward compatibility concerns)
**Goal**: Make multi-day workflows ergonomic without manual outbox/scheduler boilerplate
**Approach**: Pragmatic, minimal viable implementation

## Decision: Build Queue-Backed Version

### Why

1. **Multi-day workflows need durability** - Queue provides this naturally
2. **Eliminate boilerplate** - No manual outbox in every effect (~12 lines → 3 lines per effect)
3. **No users to break** - Can iterate freely
4. **Actually need it** - Not theoretical, actual use case

### Why Not Stay Stateless

Current architecture requires manual patterns everywhere:
```rust
effect::on::<OrderPlaced>().then(|event, ctx| async move {
    let mut tx = ctx.deps().db.begin().await?;

    // Idempotency check
    if ctx.deps().db.event_processed(event.id).await? {
        return Ok(());
    }

    // Business logic (2 lines)
    process_order(&mut tx).await?;

    // Manual outbox write
    ctx.deps().outbox.write_event(&ApprovalRequested { ... }, &mut tx).await?;

    // Mark processed
    ctx.deps().db.mark_processed(event.id, &mut tx).await?;

    tx.commit().await?;
    Ok(())
});
```

**15 lines of boilerplate per effect × 20 effects = 300 lines you don't want to write**

## Architecture

### Core Concept

```
Event → Postgres Queue → Worker Pool → Load State → Run Reducer → Save State → Run Effects → Publish Next Events
```

### Components

1. **PostgresQueue** - Durable event queue using SKIP LOCKED pattern
2. **PostgresStateStore** - Per-saga state isolation with versioning
3. **Worker Pool** - Multiple workers consuming from queue
4. **Idempotency Layer** - ProcessedEvents table prevents duplicates

### Key Design Decisions

#### 1. No Trait Abstractions (Yet)

**Don't build**:
```rust
trait EventQueue { ... }
trait StateStore { ... }
```

**Just build**:
```rust
struct PostgresQueue { ... }
struct PostgresStateStore { ... }
```

**Rationale**: YAGNI - Add traits when you need a second implementation, not before.

#### 2. Postgres Only (For Now)

**Don't build**: NATS, Kafka, SQS, Redis implementations

**Just build**: Postgres-based queue using SQL transactions

**Rationale**:
- One dependency (sqlx)
- Simple operations (one database)
- Fast enough for your use case
- Can add other backends later if needed

#### 3. No Backward Compatibility

**Delete**:
```rust
let handle = engine.activate(state);  // Old stateless API
```

**Keep only**:
```rust
engine.start_workers(4).await?;  // New queue-backed API
```

**Rationale**: You're the only user - no need for dual-mode support.

## Implementation Plan

### Phase 1: Core Queue (Week 1)

**Schema**:
```sql
-- Events queue
CREATE TABLE seesaw_events (
    id BIGSERIAL PRIMARY KEY,
    event_id UUID NOT NULL,
    parent_id UUID,
    saga_id UUID,
    event_type VARCHAR(255) NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at TIMESTAMPTZ,
    locked_until TIMESTAMPTZ,
    retry_count INT NOT NULL DEFAULT 0
);

CREATE INDEX idx_events_pending ON seesaw_events(processed_at, locked_until)
WHERE processed_at IS NULL;
```

**Core methods**:
```rust
impl PostgresQueue {
    async fn publish(&self, event: Event) -> Result<()> {
        // INSERT INTO seesaw_events
    }

    async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        // UPDATE ... FOR UPDATE SKIP LOCKED pattern
    }

    async fn ack(&self, id: i64) -> Result<()> {
        // UPDATE processed_at = NOW()
    }
}
```

**Deliverable**: Can publish and consume events from Postgres queue

### Phase 2: State Management (Week 2)

**Schema**:
```sql
-- State per saga
CREATE TABLE seesaw_state (
    saga_id UUID PRIMARY KEY,
    state JSONB NOT NULL,
    version INT NOT NULL DEFAULT 1,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Idempotency
CREATE TABLE seesaw_processed (
    event_id UUID PRIMARY KEY,
    processed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

**Core methods**:
```rust
impl PostgresStateStore<S> {
    async fn load(&self, saga_id: Uuid) -> Result<S> {
        // SELECT state FROM seesaw_state WHERE saga_id = $1
    }

    async fn save(&self, saga_id: Uuid, state: &S) -> Result<()> {
        // INSERT ... ON CONFLICT DO UPDATE
    }
}
```

**Deliverable**: Can load/save state per saga

### Phase 3: Worker Loop (Week 3)

**Core logic**:
```rust
async fn process_next_event(&self) -> Result<()> {
    // 1. Poll next event (SKIP LOCKED)
    let Some(queued) = self.queue.poll_next().await? else {
        tokio::time::sleep(Duration::from_millis(100)).await;
        return Ok(());
    };

    // 2. Check idempotency
    if self.already_processed(queued.event_id).await? {
        self.queue.ack(queued.id).await?;
        return Ok(());
    }

    // 3. Load state
    let prev_state = self.state_store.load(queued.saga_id).await?;

    // 4. Run reducer
    let next_state = self.reducers.apply(prev_state.clone(), &queued.event);

    // 5. Save state
    self.state_store.save(queued.saga_id, &next_state).await?;

    // 6. Run effects
    let ctx = EffectContext {
        prev_state,
        next_state: next_state.clone(),
        deps: self.deps.clone(),
        saga_id: queued.saga_id,
    };

    for effect in self.effects.handlers_for(&queued.event_type) {
        if let Some(next_event) = effect.handle(&queued.event, &ctx).await? {
            self.queue.publish(next_event).await?;
        }
    }

    // 7. Mark processed
    self.mark_processed(queued.event_id).await?;

    // 8. Ack
    self.queue.ack(queued.id).await?;

    Ok(())
}
```

**Deliverable**: End-to-end event processing with durability

### Total Estimate

**Lines of Code**: ~500 LOC
**Time**: 3 weeks (part-time)
**Dependencies**: sqlx, tokio, serde, anyhow

## Usage Examples

### Basic Setup

```rust
use seesaw::Engine;
use sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<()> {
    let pool = PgPool::connect(&env::var("DATABASE_URL")?).await?;

    let engine = Engine::new(pool, deps)
        .with_effect(
            effect::on::<OrderPlaced>().then(|event, ctx| async move {
                process_order(event, ctx).await?;
                Ok(ApprovalRequested {
                    order_id: event.order_id,
                    saga_id: event.saga_id,
                })
            })
        )
        .with_effect(
            effect::on::<ApprovalRequested>().then(|event, ctx| async move {
                send_approval_email(event, ctx).await?;
                Ok(())  // Observer effect
            })
        )
        .with_reducer(
            reducer::on::<OrderPlaced>().run(|state, event| {
                OrderState {
                    order_count: state.order_count + 1,
                    ..state
                }
            })
        );

    // Start 4 workers
    engine.start_workers(4).await?;

    // Publish initial event
    engine.publish(OrderPlaced { order_id: 123, saga_id: Uuid::new_v4() }).await?;

    // Keep running
    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

### Multi-Day Workflow

```rust
// Day 1: Order placed
effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().mailer.send_approval_request(event.order_id).await?;
    Ok(ApprovalRequested {
        order_id: event.order_id,
        saga_id: event.saga_id,
        expires_at: Utc::now() + Duration::days(3),
    })
});

// Day 3: Approval received (via webhook → publish to queue)
effect::on::<ApprovalReceived>().then(|event, ctx| async move {
    // Load current state from DB
    let state = ctx.next_state();

    // Process approval
    finalize_order(event.order_id, ctx).await?;

    Ok(OrderCompleted {
        order_id: event.order_id,
        saga_id: event.saga_id,
    })
});
```

**Key**: Events persist in queue between Day 1 and Day 3. Workers can restart, application can redeploy. State is durable in Postgres.

## What to Skip

### Don't Build (YAGNI)

1. **Trait Abstractions**
   - Only needed when you have 2+ implementations
   - Add traits when you add NATS/Redis/etc.

2. **Multiple Queue Backends**
   - Postgres is sufficient
   - Add NATS/Kafka if you need lower latency or multi-region

3. **In-Memory Mode**
   - Postgres is fast enough for local dev
   - Connection pooling makes it low-latency

4. **Complex Retry Strategies**
   - Simple retry count + DLQ is enough
   - Add exponential backoff if needed later

5. **Distributed Tracing**
   - Logs + event_id correlation is sufficient
   - Add OTEL if you deploy to multiple regions

6. **Admin UI**
   - SQL queries are fine for now
   - Build UI if you need it daily

### Build Later (If Needed)

1. **Redis Cache Layer**
   - If state queries become slow (>10ms)
   - Postgres query cache is probably fine

2. **Metrics/Monitoring**
   - Queue depth, processing time
   - Add when you need alerting

3. **NATS Implementation**
   - If you need multi-region
   - Or want to separate queue from database

## Performance Expectations

### Latency

**End-to-End Event Processing**:
```
Queue publish:      ~5ms (Postgres INSERT)
Worker poll:        ~5ms (Postgres SELECT with SKIP LOCKED)
State load:         ~3ms (Postgres SELECT)
Reducer:            <0.1ms (pure function)
State save:         ~5ms (Postgres UPDATE)
Effect execution:   10-100ms (your business logic)
Queue ack:          ~2ms (Postgres UPDATE)
──────────────────────────────
Total:              ~30-120ms
```

**Acceptable for**:
- Multi-day workflows (yes)
- Background jobs (yes)
- Real-time APIs (probably not)
- Event-driven microservices (yes)

### Throughput

**Single Worker**: ~10-100 events/sec (depends on effect logic)
**4 Workers**: ~40-400 events/sec
**Bottleneck**: Usually your business logic, not the queue

### Scaling

**Vertical** (single database):
- Can handle 10k+ events/sec with proper indexing
- Postgres connection pool handles concurrency

**Horizontal** (multiple workers):
- Add workers trivially: `start_workers(N)`
- SKIP LOCKED prevents duplicate processing
- Linear scaling until database becomes bottleneck

**If you hit database limits**:
- Add read replicas (for state loads)
- Shard by saga_id
- Or switch to distributed queue (NATS)

## Trade-offs Accepted

### Pros
- ✅ **Durability** - Events survive restarts
- ✅ **Simplicity** - One database, SQL queries
- ✅ **Ergonomics** - No boilerplate in effects
- ✅ **Scalability** - Worker pool pattern
- ✅ **Debuggable** - SQL queries, not distributed logs

### Cons
- ❌ **Latency** - 30-120ms per event (vs <1ms stateless)
- ❌ **Complexity** - More moving parts than stateless
- ❌ **Postgres dependency** - Can't run without database

### Acceptable Because
- Multi-day workflows tolerate 100ms latency
- You need durability more than speed
- Postgres is already in your stack
- You control the codebase

## Migration from Current

### Step 1: Remove Stateless API

```diff
- pub fn activate(&self, state: S) -> Handle<S, D> { ... }
+ // Deleted - only queue-backed mode now
```

### Step 2: Change Effect Returns

Effects already return events - no change needed!

```rust
effect::on::<Event>().then(|event, ctx| async move {
    Ok(NextEvent { ... })  // Same API, different backend
});
```

### Step 3: Update Main

```diff
- let handle = engine.activate(State::default());
- handle.run(|_| Ok(Event)).await?;
- handle.settled().await?;

+ engine.start_workers(4).await?;
+ engine.publish(InitialEvent { ... }).await?;
+ tokio::signal::ctrl_c().await?;
```

### Step 4: Add Database Setup

```rust
async fn setup_database(pool: &PgPool) -> Result<()> {
    sqlx::query(include_str!("../migrations/001_queue_tables.sql"))
        .execute(pool)
        .await?;
    Ok(())
}
```

## Decision: GO

**Recommendation**: Build the queue-backed version.

**Start with**:
1. PostgresQueue (~200 LOC)
2. PostgresStateStore (~150 LOC)
3. Worker loop (~150 LOC)
4. Idempotency layer (~50 LOC)

**Total**: ~550 LOC for durable execution

**Timeline**: 3 weeks to working implementation

**First milestone**: Multi-day approval workflow example working end-to-end

## References

### Patterns Implemented
- Transactional Outbox (automatic via queue)
- SKIP LOCKED (Postgres queue polling)
- Inbox Pattern (ProcessedEvents table)
- Per-Saga State Isolation (saga_id partitioning)
- Worker Pool (multiple consumers)

### Similar Systems
- Temporal (but heavy - workflow DSL + separate service)
- Kafka Streams (but Kafka ops overhead)
- AWS Step Functions (but AWS vendor lock-in)
- River/pgmq (SQL-based job queues - similar to this)

### Why This is Better for You
- Simpler than Temporal (no workflow DSL)
- Lighter than Kafka (just Postgres)
- Not vendor-locked (pure Rust + Postgres)
- Tailored to your needs (no unused features)

## Next Steps

1. **Prototype** - Build minimal version in 1 week
2. **Validate** - Run multi-day workflow example
3. **Measure** - Confirm latency acceptable
4. **Iterate** - Add features as needed

**Start**: Create `crates/seesaw/src/queue.rs` with PostgresQueue
