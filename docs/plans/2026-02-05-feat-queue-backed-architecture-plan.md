---
title: Queue-Backed Architecture for Seesaw
type: feat
date: 2026-02-05
status: draft
context: Single-developer project, pragmatic approach
---

# Queue-Backed Architecture for Seesaw

## Context

**Target Scale**: Millions of users, 1000+ events/sec
**Goal**: Production-grade event-driven platform for multi-day workflows
**Approach**: Battle-tested architecture, assume high traffic from day one
**Philosophy**: Build for scale now, not later (migrations at scale are expensive)

## Critical Semantic Decisions & Invariants

These decisions are **locked** and form the semantic contract of the system. Changing them post-launch breaks production systems.

### 1. Effect State Semantics (Two-Phase)

**Decision**: Effects see **latest state**, not snapshot state.

**Rationale**:
- Storing `prev_state`/`next_state` JSON per effect execution doesn't scale (15GB/month → bloat)
- Effect workers already load fresh state from `seesaw_state` before execution
- Effects are side-effect handlers, not time-travelers - they operate on current reality

**API Contract**:
```rust
effect::on::<OrderPlaced>()
    .id("send_email")
    .then(|event, ctx| async move {
        // ctx.state() returns LATEST state from seesaw_state table
        // NOT the state at the time the event was published
        let current_state = ctx.state();  // Loaded fresh from DB

        ctx.deps().mailer.send(current_state.user_email).await?;
        Ok(())
    })
```

**Invariant**: `ctx.prev_state()` and `ctx.next_state()` are **removed** from effect context. Effects only have `ctx.state()` (current).

---

### 2. Effect → Event Atomicity

**Decision**: Emitted events are inserted **atomically** with effect completion using deterministic event IDs.

**Implementation**:
```rust
// Effect emits event
let next_event = PaymentCharged { order_id };

// Framework generates deterministic ID
let emitted_event_id = hash(effect_execution.event_id, effect_execution.effect_id, event_type);

let mut tx = pool.begin().await?;

// 1. Insert emitted event (deterministic ID = idempotent)
sqlx::query!(
    "INSERT INTO seesaw_events (event_id, saga_id, event_type, payload, parent_id)
     VALUES ($1, $2, $3, $4, $5)
     ON CONFLICT (event_id) DO NOTHING",  // ← Already published if crash+retry
    emitted_event_id,
    saga_id,
    type_name::<PaymentCharged>(),
    json!(next_event),
    current_event_id
).execute(&mut *tx).await?;

// 2. Mark effect complete (atomic with event insert)
sqlx::query!(
    "UPDATE seesaw_effect_executions
     SET status = 'completed', completed_at = NOW()
     WHERE event_id = $1 AND effect_id = $2",
    event_id,
    effect_id
).execute(&mut *tx).await?;

tx.commit().await?;  // ← Both succeed or both fail
```

**Invariant**: Effects cannot publish duplicate events on crash+retry.

---

### 3. External Idempotency

**Decision**: Add `process_with_id(event_id, event)` for webhook deduplication.

**API**:
```rust
// Webhook handler
async fn handle_stripe_webhook(payload: StripeWebhook) -> Result<()> {
    // Use webhook ID as event_id (Stripe guarantees uniqueness)
    engine.process_with_id(
        Uuid::parse_str(&payload.id)?,  // ← Stripe's idempotency key
        OrderPlaced { ... }
    ).await?;

    Ok(())
}
```

**Schema**:
```sql
-- Unique constraint prevents duplicate webhook processing
CREATE UNIQUE INDEX idx_events_event_id ON seesaw_events(event_id);
```

**Invariant**: `process()` generates random UUIDs (dev use). `process_with_id()` enforces uniqueness (production webhooks).

---

### 4. Ordering Semantics

**Decision**: **Per-saga FIFO** with priority override.

**Guarantees**:
- Events within a saga process in creation order (`created_at ASC`)
- Priority can **reorder across sagas** but not within a saga
- Reducers must be **commutative** if they depend on global state

**Implementation**:
```sql
-- Worker query enforces per-saga FIFO with advisory locks
SELECT
    pg_advisory_xact_lock(hashtext(saga_id::text)),  -- ← Lock saga for transaction
    *
FROM seesaw_events
WHERE processed_at IS NULL
  AND (locked_until IS NULL OR locked_until < NOW())
ORDER BY created_at ASC  -- FIFO within available events
LIMIT 1
FOR UPDATE SKIP LOCKED;
```

**How it works**:
1. `pg_advisory_xact_lock(hash(saga_id))` acquires per-saga lock for transaction duration
2. Only one worker can process events from a given saga at a time
3. Other workers skip locked sagas (via SKIP LOCKED) and grab events from different sagas
4. Ensures strict per-saga FIFO ordering

**Non-Guarantee**: No **cross-saga** ordering. `saga_A.event_1` and `saga_B.event_1` may process in any order.

**Invariant**: If your reducer accesses global state (e.g., `total_orders++`), you need pessimistic locking or make it commutative.

---

### 5. Ownership Model

**Decision**: **Runtime** owns both engine + queue + workers.

**Structure**:
```rust
pub struct Runtime {
    engine: Engine<S, D>,
    queue: Arc<PostgresQueue>,
    event_workers: JoinSet<()>,   // Owned by Runtime
    effect_workers: JoinSet<()>,  // Owned by Runtime
}

impl Runtime {
    pub fn spawn_workers(&mut self, event_count: usize, effect_count: usize) {
        // Spawn event workers (state transitions)
        for _ in 0..event_count {
            let queue = self.queue.clone();
            let engine = self.engine.clone();
            self.event_workers.spawn(async move {
                event_worker_loop(queue, engine).await
            });
        }

        // Spawn effect workers (side effects)
        for _ in 0..effect_count {
            let queue = self.queue.clone();
            let engine = self.engine.clone();
            self.effect_workers.spawn(async move {
                effect_worker_loop(queue, engine).await
            });
        }
    }
}
```

**Invariant**: Queue does not own workers. Runtime orchestrates the system.

---

### 6. Trait Abstraction Strategy

**Decision**: **No trait abstractions** for Queue/StateStore in v1.

**Rationale**:
- Queue must be generic over `Q: Queue` to be object-safe: `publish(envelope: EventEnvelope)` breaks type inference
- Postgres is the control plane - Redis/NATS are orthogonal (data plane, not queue replacement)
- Adding traits pre-emptively creates false flexibility

**Concrete Types**:
```rust
pub struct Runtime<S, D> {
    engine: Engine<S, D>,
    queue: PostgresQueue,          // ← Concrete, not dyn Queue
    state_store: PostgresStateStore<S>,  // ← Concrete
}
```

**Future**: If we add Redis queue, it's a **separate crate** (`seesaw-redis`), not a trait impl.

---

### 7. Scale Reality Check

**Throughput Estimates** (Postgres + Worker Pool):

| Component | Throughput | Bottleneck | Mitigation |
|-----------|------------|------------|------------|
| **Event Workers** (Reducers) | 500-1000 events/sec | `FOR UPDATE` lock contention | Partition by `saga_id` |
| **Effect Workers** (IO) | 50-200 effects/sec | External API latency | Increase worker count (20+) |
| **Database Writes** | 5000 events/sec | WAL write speed | Async commit + batching |
| **Storage Growth** | ~15GB/month | Disk space | Partition tables + retention policy |

**Scaling Path**:
1. **0-100k users**: Single Postgres instance, 2 event workers, 10 effect workers
2. **100k-1M users**: Read replicas + partition by `saga_id` hash
3. **1M+ users**: Dedicated queue shards + distributed tracing

**Invariant**: Postgres is the **control plane**. High-throughput data plane (e.g., analytics events) belongs in NATS/Kafka, not this queue.

---

### 8. Schema Fixes

**Required Changes**:
```sql
-- 1. Unique constraint on event_id (idempotency)
CREATE UNIQUE INDEX idx_events_event_id ON seesaw_events(event_id);

-- 2. Add saga_id to effect_executions (efficient queries)
ALTER TABLE seesaw_effect_executions ADD COLUMN saga_id UUID NOT NULL;
CREATE INDEX idx_effect_executions_saga ON seesaw_effect_executions(saga_id);

-- 3. Fix claimed_at default (should be NULL for pending rows)
ALTER TABLE seesaw_effect_executions
ALTER COLUMN claimed_at DROP DEFAULT,
ALTER COLUMN claimed_at SET DEFAULT NULL;

-- 4. Retention policy (delete after 30 days)
CREATE TABLE seesaw_events_archive (LIKE seesaw_events INCLUDING ALL);

-- Daily cron job
INSERT INTO seesaw_events_archive
SELECT * FROM seesaw_events WHERE processed_at < NOW() - INTERVAL '30 days';

DELETE FROM seesaw_events WHERE processed_at < NOW() - INTERVAL '30 days';
```

**Invariant**: Event and effect data is retained for 30 days for debugging, then archived.

---

## Summary of Invariants

| Invariant | Contract |
|-----------|----------|
| **State Semantics** | Effects see latest state only (no `prev_state`/`next_state`) |
| **Atomicity** | Effect completion + event emission happen atomically |
| **Idempotency** | `event_id` is unique, crash+retry safe |
| **Ordering** | Per-saga FIFO, no cross-saga guarantees |
| **Ownership** | Runtime owns workers, queue is passive |
| **Abstractions** | Concrete types (Postgres), no traits in v1 |
| **Scale** | Control plane queue (500-1000 events/sec), not data plane |
| **Retention** | 30-day retention with archival |

These decisions are **locked**. Any change requires a migration plan and may break production systems.

---

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
External Event → engine.process() → Queue.publish()
                                         ↓
                                 Event Worker (owned by Runtime)
                                         ↓
                              Poll → Engine.process_event()
                                         ↓
                    Load State → Reducers → Save State
                                         ↓
                                   Run Effects:
                                    ├─ Inline effects (execute immediately)
                                    │  └─ Emit events → Queue.publish()
                                    └─ Queued effects (persist to DB)
                                       └─ INSERT seesaw_effect_executions
                                              ↓
                                    Effect Worker (owned by Runtime)
                                              ↓
                                    Poll → Execute → Emit events
```

### Components

1. **Runtime** - Orchestrates the system (owns workers + coordinates engine + queue)
   - Spawns event workers and effect workers
   - Manages lifecycle (startup, shutdown, graceful drain)
2. **PostgresQueue** - Passive durable storage using SKIP LOCKED pattern
   - Stores events and effect intents
   - Workers poll from it, but it doesn't own them
3. **Engine** - Business logic (reducers + effects)
   - Receives queue reference via dependency injection
   - Stateless and reusable
4. **PostgresStateStore** - Per-saga state isolation with versioning (embedded in PostgresQueue)
5. **Idempotency Layer** - seesaw_events.event_id + seesaw_effect_executions tables

### Effect Execution Model: Inline vs Queued

**Critical Design Decision**: Effects are **inline by default** (zero overhead), automatically queued only when needed.

#### Inline Effects (Default)

Run immediately during event processing, no Postgres overhead:

```rust
// ✅ Inline - runs in event worker, no DB write
effect::on::<OrderPlaced>()
    .id("log_order")
    .then(|event, ctx| async move {
        tracing::info!("Order placed: {}", event.order_id);
        Ok(())  // Fast, synchronous-style logic
    });

// ✅ Inline - emit next event immediately
effect::on::<OrderPlaced>()
    .id("request_approval")
    .then(|event, ctx| async move {
        Ok(ApprovalRequested { order_id: event.order_id })
    });
```

**When inline**:
- No `.delayed()`, `.retry()`, `.timeout()`, `.priority()`, or `.queued()` modifiers
- Executes during event processing (same transaction as state update)
- Emitted events published immediately
- Zero Postgres overhead (no `seesaw_effect_executions` write)

#### Queued Effects (Opt-In)

Persisted to `seesaw_effect_executions`, polled by effect workers:

```rust
// ✅ Queued - has .delayed() → requires persistence
effect::on::<OrderPlaced>()
    .id("send_reminder")
    .delayed(Duration::from_days(7))
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_reminder(&event).await?;
        Ok(ReminderSent { order_id: event.order_id })
    });

// ✅ Queued - has .retry() → needs durability for crash recovery
effect::on::<OrderPlaced>()
    .id("charge_payment")
    .retry(5)
    .timeout(Duration::from_secs(60))
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    });

// ✅ Queued - explicit .queued() for long-running IO
effect::on::<OrderPlaced>()
    .id("generate_report")
    .queued()  // Force queued execution
    .timeout(Duration::from_secs(120))
    .then(|event, ctx| async move {
        let report = ctx.deps().reporter.generate(&event).await?;
        Ok(ReportGenerated { order_id: event.order_id })
    });
```

**When queued** (any of these triggers persistence):
- Has `.delayed(duration)` - schedule for future execution
- Has `.retry(n)` where n > 1 - needs crash recovery
- Has `.timeout(duration)` - implies potentially slow IO
- Has `.priority(n)` - needs priority queue scheduling
- Has `.queued()` - explicit opt-in for long-running work

### Two-Phase Worker Architecture

**Phase 1: Event Workers** (Fast - State + Inline Effects)
```
1. Poll seesaw_events (SKIP LOCKED + advisory lock for FIFO)
2. Load state → Run reducers → Save state
3. For each effect:
   - If inline: Execute immediately, emit events
   - If queued: Insert into seesaw_effect_executions
4. Mark event processed
```

**Phase 2: Effect Workers** (Slow - Queued Effects Only)
```
1. Poll seesaw_effect_executions (where pending AND execute_at <= NOW)
2. Execute effect (retry on failure, respect timeout)
3. Mark completed, emit next events
```

**Why Split?**
- ✅ **Zero overhead for simple effects** - Inline execution, no DB write
- ✅ **Isolation** - Slow third-party APIs don't block state updates
- ✅ **Scalability** - Run 2 event workers, 20 effect workers (queued effects are the bottleneck)
- ✅ **Durability** - Queued effects survive crashes, support retries and delays
- ✅ **Pay-per-use** - Only persist effects that need durability

### Key Design Decisions

#### 1. Trait Abstractions (Pluggable Backends)

**Decision**: Use traits for `Queue` and `StateStore` to enable pluggable backends.

**Rationale**:
- ✅ **Use existing infrastructure** - Swap in NATS, Redis, custom queue implementations
- ✅ **Testability** - In-memory implementations for fast tests without Postgres
- ✅ **Flexibility** - SQLite for embedded, Postgres for production, custom for special cases
- ✅ **Separation of concerns** - Engine doesn't know about storage implementation
- Standard Rust pattern - abstractions via traits

**Queue Trait**:
```rust
#[async_trait]
pub trait Queue: Send + Sync + 'static {
    async fn publish(&self, envelope: EventEnvelope) -> Result<()>;
    async fn poll_event(&self) -> Result<Option<EventEnvelope>>;
    async fn poll_effect(&self) -> Result<Option<EffectIntent>>;
    async fn ack_event(&self, event_id: Uuid) -> Result<()>;
    async fn ack_effect(&self, event_id: Uuid, effect_id: &str) -> Result<()>;
}

#[async_trait]
pub trait StateStore: Send + Sync + 'static {
    async fn load<S: DeserializeOwned + Send>(&self, saga_id: Uuid) -> Result<Option<S>>;
    async fn save<S: Serialize + Send>(&self, saga_id: Uuid, state: &S, version: i32) -> Result<()>;
}
```

**Postgres Implementation** (ships with seesaw):
```rust
pub struct PostgresQueue {
    pool: PgPool,
}

#[async_trait]
impl Queue for PostgresQueue {
    async fn publish(&self, envelope: EventEnvelope) -> Result<()> {
        sqlx::query!("INSERT INTO seesaw_events ...").execute(&self.pool).await?;
        Ok(())
    }
    // ... other methods
}

pub struct PostgresStateStore {
    pool: PgPool,
}

#[async_trait]
impl StateStore for PostgresStateStore {
    async fn load<S: DeserializeOwned + Send>(&self, saga_id: Uuid) -> Result<Option<S>> {
        let row = sqlx::query!("SELECT state FROM seesaw_state WHERE saga_id = $1", saga_id)
            .fetch_optional(&self.pool).await?;
        match row {
            Some(r) => Ok(Some(serde_json::from_value(r.state)?)),
            None => Ok(None),
        }
    }
    // ...
}
```

**Engine is Generic**:
```rust
pub struct Engine<S, D, Q: Queue, St: StateStore> {
    queue: Arc<Q>,
    state_store: Arc<St>,
    deps: Arc<D>,
    // ...
}

impl<S, D, Q: Queue, St: StateStore> Engine<S, D, Q, St> {
    pub fn new(deps: D) -> Self;
    pub fn with_queue(self, queue: Arc<Q>) -> Self;
    pub fn with_state(self, state: Arc<St>) -> Self;
    // ...
}
```

**User Code** (type inference works):
```rust
// Production: Postgres
let queue = Arc::new(PostgresQueue::new(pool));
let state = Arc::new(PostgresStateStore::new(pool));
let engine = Engine::with_deps(deps)
    .with_queue(queue)
    .with_state(state);

// Tests: In-memory
let queue = Arc::new(InMemoryQueue::new());
let state = Arc::new(InMemoryStateStore::new());
let engine = Engine::with_deps(deps)
    .with_queue(queue)
    .with_state(state);

// Custom: NATS + Redis
let queue = Arc::new(NatsQueue::new(nc));
let state = Arc::new(RedisStateStore::new(client));
let engine = Engine::with_deps(deps)
    .with_queue(queue)
    .with_state(state);
```

#### 2. Ship Postgres + In-Memory Implementations

**v1 includes**:
- `PostgresQueue` + `PostgresStateStore` (production)
- `InMemoryQueue` + `InMemoryStateStore` (testing)

**Users can implement**:
- `NatsQueue` (use your existing NATS infrastructure)
- `RedisStateStore` (use your existing Redis cluster)
- `SqliteQueue` (embedded deployments)
- Custom implementations (enterprise requirements)

**Rationale**:
- Postgres is production-ready and well-tested (ships by default)
- In-memory enables fast tests without external dependencies
- Trait abstraction allows users to leverage existing infrastructure
- No vendor lock-in

#### 2. No Backward Compatibility

**v0.7 API** (stateless):
```rust
let handle = engine.activate(state);
handle.run(|_| Ok(Event)).await?;
handle.settled().await?;
```

**New API** (queue-backed, Runtime owns workers):
```rust
let queue = Arc::new(PostgresQueue::new(pool));
let engine = Engine::with_deps(deps).with_queue(queue.clone());
let mut runtime = Runtime::new(engine.clone(), queue);
runtime.spawn_workers(2, 4);  // 2 event workers, 4 effect workers

// Process external events (webhooks, HTTP handlers)
engine.process(OrderPlaced { ... }).await?;

// Shutdown
runtime.shutdown().await?;
```

**Rationale**: You're the only user - no need for dual-mode support. Runtime owns workers, queue is passive storage.

#### 4. Framework-Guaranteed Idempotency (Critical!)

**Problem**: Requiring users to handle idempotency manually is a DX footgun:
```rust
// BAD: Easy to forget, duplicate side effects!
effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().mailer.send_email(event.email).await?;  // Could send twice!
    Ok(EmailSent { ... })
});
```

**Solution**: Framework guarantees idempotency automatically.

**API Design**:
```rust
engine.with_effect(
    effect::on::<OrderPlaced>()
        .id("send_welcome_email")  // ← Required! Compile error if missing
        .then(|event, ctx| async move {
            // Write naturally - framework handles idempotency!
            ctx.deps().mailer.send(event.email).await?;
            Ok(EmailSent { order_id: event.order_id })
        })
)
```

**How it works**:
1. Effect ID required at compile time (type-state pattern)
2. Track executions in `seesaw_effect_executions` table
3. Cache results, skip on replay
4. Provide `ctx.idempotency_key` for external APIs

**Schema**:
```sql
CREATE TABLE seesaw_effect_executions (
    event_id UUID NOT NULL,
    effect_id VARCHAR(255) NOT NULL,
    saga_id UUID NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'pending',
    result JSONB,
    error TEXT,
    attempts INT NOT NULL DEFAULT 0,

    -- Event payload (survives 30-day retention deletion)
    event_type VARCHAR(255) NOT NULL,
    event_payload JSONB NOT NULL,
    parent_event_id UUID,

    -- Execution properties
    execute_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    timeout_seconds INT NOT NULL DEFAULT 30,
    max_attempts INT NOT NULL DEFAULT 3,
    priority INT NOT NULL DEFAULT 10,

    claimed_at TIMESTAMPTZ,  -- NULL until claimed
    completed_at TIMESTAMPTZ,
    last_attempted_at TIMESTAMPTZ,
    PRIMARY KEY (event_id, effect_id)
);

CREATE INDEX idx_effect_executions_event ON seesaw_effect_executions(event_id);
```

**Compile-Time Enforcement**:
```rust
// Type-state pattern ensures .id() is called
pub struct EffectBuilder<E, HasId> {
    effect_id: Option<String>,
    _phantom: PhantomData<(E, HasId)>,
}

pub struct NoId;
pub struct HasIdTag;

impl<E> EffectBuilder<E, NoId> {
    pub fn id(self, id: &str) -> EffectBuilder<E, HasIdTag> {
        EffectBuilder {
            effect_id: Some(id.to_string()),
            _phantom: PhantomData,
        }
    }
}

// Can only call .then() if HasIdTag
impl<E> EffectBuilder<E, HasIdTag> {
    pub fn then<F>(self, handler: F) -> Effect<E> {
        Effect {
            id: self.effect_id.unwrap(),  // Safe - HasIdTag guarantees it
            handler: Box::new(handler),
        }
    }
}

// Forget .id() → Compile error!
effect::on::<OrderPlaced>()
    .then(|event, ctx| { ... })  // ← Error: id() required!
```

**Runtime Idempotency**:
```rust
async fn run_effect_idempotent(
    &self,
    event_id: Uuid,
    effect: &Effect,
    event: &Event,
    ctx: &Context
) -> Result<Option<Event>> {
    let mut tx = self.pool.begin().await?;

    // Check if already executed
    let cached = sqlx::query!(
        "INSERT INTO seesaw_effect_executions (event_id, effect_id, status)
         VALUES ($1, $2, 'executing')
         ON CONFLICT (event_id, effect_id)
         DO UPDATE SET last_attempted_at = NOW()
         RETURNING status, result, error, attempts",
        event_id,
        effect.id
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    // Already completed - return cached result!
    if cached.status == "completed" {
        return Ok(cached.result.map(|r| deserialize(&r)?));
    }

    // Failed too many times - don't retry
    if cached.status == "failed" && cached.attempts >= 3 {
        return Err(anyhow!("Effect permanently failed: {}", cached.error));
    }

    // Execute effect (outside transaction - can take time)
    match effect.handler.handle(event, ctx).await {
        Ok(result) => {
            // Store success
            sqlx::query!(
                "UPDATE seesaw_effect_executions
                 SET status = 'completed', result = $1, completed_at = NOW()
                 WHERE event_id = $2 AND effect_id = $3",
                Json(&result),
                event_id,
                effect.id
            )
            .execute(&self.pool)
            .await?;

            Ok(result)
        }
        Err(e) => {
            // Store failure (will retry up to 3 times)
            sqlx::query!(
                "UPDATE seesaw_effect_executions
                 SET status = 'failed', error = $1, attempts = attempts + 1
                 WHERE event_id = $2 AND effect_id = $3",
                e.to_string(),
                event_id,
                effect.id
            )
            .execute(&self.pool)
            .await?;

            Err(e)
        }
    }
}
```

**Guarantees Provided**:
- ✅ Effects execute at-least-once (queue redelivery)
- ✅ Results cached in database (idempotent replay)
- ✅ Effect ID required at compile time (no footgun)
- ✅ Idempotency key provided in context (for external APIs)
- ✅ Automatic retry with limit (3 attempts default)

**For External APIs**:
```rust
effect::on::<OrderPlaced>()
    .id("charge_payment")
    .then(|event, ctx| async move {
        // Framework provides idempotency key = hash(event_id + effect_id)
        ctx.deps().stripe.charge(
            amount,
            &ctx.idempotency_key  // ← Framework provides this!
        ).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    })
```

**Comparison**:

| Approach | LOC | Footguns | Guarantees |
|----------|-----|----------|------------|
| **Manual** | 15 lines per effect | High (easy to forget) | None |
| **Framework** | 6 lines per effect | None (compile error) | Idempotency guaranteed |

**Rationale**: This is critical infrastructure. Users should never have to think about idempotency.

## Implementation Plan

### Phase 1: Core Queue (Week 1)

**Schema**:
```sql
-- Events queue (envelope carries saga_id as metadata)
CREATE TABLE seesaw_events (
    id BIGSERIAL PRIMARY KEY,
    event_id UUID NOT NULL,
    parent_id UUID,
    saga_id UUID NOT NULL,           -- ← Envelope metadata, not in user events
    event_type VARCHAR(255) NOT NULL,
    payload JSONB NOT NULL,          -- User event data (no saga_id inside)
    hops INT NOT NULL DEFAULT 0,     -- ← Infinite loop protection
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at TIMESTAMPTZ,
    locked_until TIMESTAMPTZ,
    retry_count INT NOT NULL DEFAULT 0
    -- No priority on events - priority is on effects only
);

-- Idempotency: prevent duplicate event_ids (webhooks, crash+retry)
CREATE UNIQUE INDEX idx_events_event_id ON seesaw_events(event_id);

-- Per-saga FIFO with advisory locks (no separate priority index needed)
CREATE INDEX idx_events_pending
ON seesaw_events(created_at ASC)
WHERE processed_at IS NULL;

-- LISTEN/NOTIFY trigger for .wait() pattern (CQRS support)
CREATE OR REPLACE FUNCTION notify_saga_event()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'seesaw_saga_' || NEW.saga_id::text,
        json_build_object(
            'event_type', NEW.event_type,
            'payload', NEW.payload
        )::text
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER seesaw_events_notify
    AFTER INSERT ON seesaw_events
    FOR EACH ROW
    EXECUTE FUNCTION notify_saga_event();
```

**Core methods**:
```rust
impl PostgresQueue {
    async fn publish(&self, event: Event) -> Result<()> {
        // INSERT INTO seesaw_events
    }

    async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        // Per-saga FIFO with advisory locks
        sqlx::query_as!(
            QueuedEvent,
            "SELECT
                pg_advisory_xact_lock(hashtext(saga_id::text)),
                id, event_id, saga_id, event_type, payload, hops
             FROM seesaw_events
             WHERE processed_at IS NULL
               AND (locked_until IS NULL OR locked_until < NOW())
             ORDER BY created_at ASC
             LIMIT 1
             FOR UPDATE SKIP LOCKED"
        )
        .fetch_optional(&self.pool)
        .await
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

-- Event processing ledger (separate from queue)
-- Purpose: Atomic claim + processing phase tracking + historical record
-- Why not just use seesaw_events.processed_at?
--   1. Atomic claim: Worker inserts here at transaction start (ON CONFLICT = already claimed)
--   2. Phase tracking: Tracks state commit vs full completion separately
--   3. Audit trail: Persists even after event deleted from queue (30-day retention)
CREATE TABLE seesaw_processed (
    event_id UUID PRIMARY KEY,
    saga_id UUID NOT NULL,
    state_committed_at TIMESTAMPTZ,  -- When Phase 1 (reducers) completed
    completed_at TIMESTAMPTZ,        -- When Phase 2 (all effects) completed
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Effect idempotency (framework-guaranteed)
CREATE TABLE seesaw_effect_executions (
    event_id UUID NOT NULL,
    effect_id VARCHAR(255) NOT NULL,
    saga_id UUID NOT NULL,  -- ← For efficient per-saga queries
    status VARCHAR(50) NOT NULL DEFAULT 'pending',  -- pending, executing, completed, failed
    result JSONB,
    error TEXT,
    attempts INT NOT NULL DEFAULT 0,

    -- Event payload (for delayed effects >30 days)
    event_type VARCHAR(255) NOT NULL,
    event_payload JSONB NOT NULL,  -- ← Copied from seesaw_events, survives retention deletion
    parent_event_id UUID,           -- For causality tracking

    -- Execution properties (from effect builder)
    execute_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- When to execute (for .delayed())
    timeout_seconds INT NOT NULL DEFAULT 30,         -- Max execution time
    max_attempts INT NOT NULL DEFAULT 3,             -- Retry limit
    priority INT NOT NULL DEFAULT 10,                -- Execution priority

    claimed_at TIMESTAMPTZ,  -- NULL until claimed by worker
    completed_at TIMESTAMPTZ,
    last_attempted_at TIMESTAMPTZ,
    PRIMARY KEY (event_id, effect_id)
);

-- Worker polling: find next effect to execute
CREATE INDEX idx_effect_executions_pending
ON seesaw_effect_executions(priority DESC, execute_at ASC)
WHERE status = 'pending' AND execute_at <= NOW();

-- Lookup effects by event (for debugging)
CREATE INDEX idx_effect_executions_event ON seesaw_effect_executions(event_id);

-- Lookup effects by saga (for per-saga queries)
CREATE INDEX idx_effect_executions_saga ON seesaw_effect_executions(saga_id);

-- Retry monitoring (find failing effects)
CREATE INDEX idx_effect_executions_status ON seesaw_effect_executions(status, attempts);
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

### Phase 3: Two-Phase Worker Architecture (Week 3)

**Core logic** (split event and effect processing):

#### Phase 1 Worker: Event Processing (Fast)

```rust
async fn event_worker_loop(&self) -> Result<()> {
    loop {
        // 1. Poll next event
        let Some(event) = self.queue.poll_next().await? else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };

        // 2. Infinite loop protection
        if event.hops >= 50 {
            self.move_to_dlq(event, "infinite_loop").await?;
            self.queue.ack(event.id).await?;
            continue;
        }

        // 3. Atomic event idempotency claim
        let mut tx = self.pool.begin().await?;

        let claimed = sqlx::query!(
            "INSERT INTO seesaw_processed (event_id, saga_id)
             VALUES ($1, $2)
             ON CONFLICT (event_id) DO NOTHING
             RETURNING event_id",
            event.event_id,
            event.saga_id
        )
        .fetch_optional(&mut *tx)
        .await?;

        if claimed.is_none() {
            tx.rollback().await?;
            self.queue.ack(event.id).await?;
            continue;
        }

        // 4. Initialize or load state
        sqlx::query!(
            "INSERT INTO seesaw_state (saga_id, state, version)
             VALUES ($1, $2, 1)
             ON CONFLICT (saga_id) DO NOTHING",
            event.saga_id,
            Json(&S::default())
        )
        .execute(&mut *tx)
        .await?;

        let (prev_state, version) = sqlx::query!(
            "SELECT state, version FROM seesaw_state
             WHERE saga_id = $1 FOR UPDATE",
            event.saga_id
        )
        .fetch_one(&mut *tx)
        .await?;

        // 5. Run reducer (pure, fast)
        let next_state = self.reducers.apply(prev_state.clone(), &event);

        // 6. Save state
        sqlx::query!(
            "UPDATE seesaw_state
             SET state = $1, version = $2, updated_at = NOW()
             WHERE saga_id = $3 AND version = $4",
            Json(&next_state),
            version + 1,
            event.saga_id,
            version
        )
        .execute(&mut *tx)
        .await?;

        // 7. Insert effect execution intents (with event payload for retention safety)
        for effect in self.effects.handlers_for(&event.event_type) {
            sqlx::query!(
                "INSERT INTO seesaw_effect_executions (
                    event_id, effect_id, saga_id, status,
                    event_type, event_payload, parent_event_id,
                    execute_at, timeout_seconds, max_attempts, priority
                 )
                 VALUES ($1, $2, $3, 'pending', $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT DO NOTHING",
                event.event_id,
                effect.id,
                event.saga_id,
                event.event_type,
                Json(&event.payload),  // ← Copy payload (survives 30-day deletion)
                event.parent_id,
                Utc::now() + effect.config.delay,
                effect.config.timeout.as_secs(),
                effect.config.max_attempts,
                effect.config.priority
            )
            .execute(&mut *tx)
            .await?;
        }

        // 8. Commit (state + effect intents atomic)
        tx.commit().await?;

        // 9. Ack event
        self.queue.ack(event.id).await?;
    }
}
```

#### Phase 2 Worker: Effect Execution (Slow)

```rust
async fn effect_worker_loop(&self) -> Result<()> {
    loop {
        // 1. Poll next ready effect
        let Some(pending) = sqlx::query!(
            "UPDATE seesaw_effect_executions
             SET status = 'executing', claimed_at = NOW()
             WHERE (event_id, effect_id) = (
                 SELECT event_id, effect_id
                 FROM seesaw_effect_executions
                 WHERE status = 'pending'
                 AND execute_at <= NOW()
                 ORDER BY priority DESC, execute_at ASC
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING *"
        )
        .fetch_optional(&self.pool)
        .await? else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };

        // 2. Event payload is already in pending (survives retention deletion)
        // No need to query seesaw_events - it might have been deleted after 30 days
        let event_payload = pending.event_payload;
        let saga_id = pending.saga_id;

        // Get hops from parent event (if still exists, otherwise default to 0)
        let hops = sqlx::query_scalar!(
            "SELECT hops FROM seesaw_events WHERE event_id = $1",
            pending.parent_event_id
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0);

        // 3. Get state for context
        let state = sqlx::query!(
            "SELECT state FROM seesaw_state WHERE saga_id = $1",
            saga_id
        )
        .fetch_one(&self.pool)
        .await?;

        // 4. Build context (latest state only)
        let ctx = EffectContext {
            state: Arc::new(state),  // Wrap in Arc for cheap cloning (safe across .await)
            deps: self.deps.clone(),
            saga_id: saga_id,
            event_id: pending.event_id,
            idempotency_key: Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!("{}-{}", pending.event_id, pending.effect_id).as_bytes()
            ).to_string(),
        };

        // 5. Execute effect with timeout
        let effect = self.effects.get(&pending.effect_id);
        let result = tokio::time::timeout(
            Duration::from_secs(pending.timeout_seconds as u64),
            effect.handler.handle(&event_payload, &ctx)
        ).await;

        match result {
            Ok(Ok(Some(next_event))) => {
                // ATOMIC: Insert event + mark effect complete in single transaction
                let mut tx = self.pool.begin().await?;

                // Generate deterministic event_id
                let next_event_id = Uuid::new_v5(
                    &NAMESPACE_SEESAW,
                    format!("{}-{}-{}", pending.event_id, pending.effect_id,
                            std::any::type_name_of_val(&next_event)).as_bytes()
                );

                // Insert emitted event (idempotent via event_id unique constraint)
                sqlx::query!(
                    "INSERT INTO seesaw_events
                     (event_id, parent_id, saga_id, event_type, payload, hops)
                     VALUES ($1, $2, $3, $4, $5, $6)
                     ON CONFLICT (event_id) DO NOTHING",
                    next_event_id,
                    pending.event_id,
                    saga_id,
                    std::any::type_name_of_val(&next_event),
                    Json(&next_event),
                    hops + 1
                )
                .execute(&mut *tx)
                .await?;

                // Mark effect complete (same transaction)
                sqlx::query!(
                    "UPDATE seesaw_effect_executions
                     SET status = 'completed', completed_at = NOW()
                     WHERE event_id = $1 AND effect_id = $2",
                    pending.event_id,
                    pending.effect_id
                )
                .execute(&mut *tx)
                .await?;

                tx.commit().await?;  // Both succeed or both fail
            }
            Ok(Ok(None)) => {
                // Observer effect
                sqlx::query!(
                    "UPDATE seesaw_effect_executions
                     SET status = 'completed', completed_at = NOW()
                     WHERE event_id = $1 AND effect_id = $2",
                    pending.event_id,
                    pending.effect_id
                )
                .execute(&self.pool)
                .await?;
            }
            Ok(Err(e)) | Err(_) => {
                // Effect failed or timed out
                let attempts = pending.attempts + 1;
                if attempts >= pending.max_attempts {
                    // Move to DLQ
                    self.move_effect_to_dlq(pending, e.to_string()).await?;
                } else {
                    // Retry
                    sqlx::query!(
                        "UPDATE seesaw_effect_executions
                         SET status = 'pending', attempts = $1
                         WHERE event_id = $2 AND effect_id = $3",
                        attempts,
                        pending.event_id,
                        pending.effect_id
                    )
                    .execute(&self.pool)
                    .await?;
                }
            }
        }
    }
}
```

**Deliverable**: Two-phase event processing with durability, isolation, and scalability

### Phase 3 (Old - Single Worker)
```rust
async fn process_next_event(&self) -> Result<()> {
    // 1. Poll next event (SKIP LOCKED)
    let Some(queued) = self.queue.poll_next().await? else {
        tokio::time::sleep(Duration::from_millis(100)).await;
        return Ok(());
    };

    // 2. Atomic event idempotency claim
    let mut tx = self.pool.begin().await?;

    let claimed = sqlx::query!(
        "INSERT INTO seesaw_processed (event_id, saga_id)
         VALUES ($1, $2)
         ON CONFLICT (event_id) DO NOTHING
         RETURNING event_id",
        queued.event_id,
        queued.saga_id
    )
    .fetch_optional(&mut *tx)
    .await?;

    if claimed.is_none() {
        // Another worker claimed this event
        tx.rollback().await?;
        self.queue.ack(queued.id).await?;
        return Ok(());
    }

    // 3. Load state with FOR UPDATE lock
    let (prev_state, version) = sqlx::query!(
        "SELECT state, version FROM seesaw_state
         WHERE saga_id = $1 FOR UPDATE",
        queued.saga_id
    )
    .fetch_one(&mut *tx)
    .await?;

    // 4. Run reducer (pure, fast)
    let next_state = self.reducers.apply(prev_state.clone(), &queued.event);

    // 5. Save state with version check
    sqlx::query!(
        "UPDATE seesaw_state
         SET state = $1, version = $2, updated_at = NOW()
         WHERE saga_id = $3 AND version = $4",
        Json(&next_state),
        version + 1,
        queued.saga_id,
        version
    )
    .execute(&mut *tx)
    .await?;

    // 6. Mark state committed
    sqlx::query!(
        "UPDATE seesaw_processed SET state_committed_at = NOW()
         WHERE event_id = $1",
        queued.event_id
    )
    .execute(&mut *tx)
    .await?;

    // 7. Commit transaction (state is now durable)
    tx.commit().await?;

    // 8. Run effects with framework-guaranteed idempotency
    let ctx = EffectContext {
        prev_state,
        next_state: next_state.clone(),
        deps: self.deps.clone(),
        saga_id: queued.saga_id,
        idempotency_key: format!("{}-{}", queued.event_id, "..."),  // Per-effect
    };

    for effect in self.effects.handlers_for(&queued.event_type) {
        // run_effect_idempotent checks cache and skips if already executed
        match self.run_effect_idempotent(queued.event_id, effect, &queued.event, &ctx).await {
            Ok(Some(next_event)) => {
                self.queue.publish(next_event).await?;
            }
            Ok(None) => {
                // Observer effect or cached result was None
            }
            Err(e) => {
                // Effect failed - logged but don't fail entire event
                tracing::error!("Effect {} failed: {}", effect.id, e);
            }
        }
    }

    // 9. Mark fully processed
    sqlx::query!(
        "UPDATE seesaw_processed SET completed_at = NOW()
         WHERE event_id = $1",
        queued.event_id
    )
    .execute(&self.pool)
    .await?;

    // 10. Ack queue
    self.queue.ack(queued.id).await?;

    Ok(())
}

// Helper: Run effect with idempotency guarantee
async fn run_effect_idempotent(
    &self,
    event_id: Uuid,
    effect: &Effect,
    event: &Event,
    ctx: &Context
) -> Result<Option<Event>> {
    let mut tx = self.pool.begin().await?;

    // Atomic check-and-claim
    let cached = sqlx::query!(
        "INSERT INTO seesaw_effect_executions (event_id, effect_id, status)
         VALUES ($1, $2, 'executing')
         ON CONFLICT (event_id, effect_id)
         DO UPDATE SET last_attempted_at = NOW()
         RETURNING status, result, error, attempts",
        event_id,
        effect.id
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    // Already completed - return cached!
    if cached.status == "completed" {
        return Ok(cached.result.map(|r| deserialize(&r)?));
    }

    // Failed too many times
    if cached.status == "failed" && cached.attempts >= 3 {
        return Err(anyhow!("Permanently failed"));
    }

    // Execute effect
    match effect.handler.handle(event, ctx).await {
        Ok(result) => {
            sqlx::query!(
                "UPDATE seesaw_effect_executions
                 SET status = 'completed', result = $1, completed_at = NOW()
                 WHERE event_id = $2 AND effect_id = $3",
                Json(&result),
                event_id,
                effect.id
            )
            .execute(&self.pool)
            .await?;

            Ok(result)
        }
        Err(e) => {
            sqlx::query!(
                "UPDATE seesaw_effect_executions
                 SET status = 'failed', error = $1, attempts = attempts + 1
                 WHERE event_id = $2 AND effect_id = $3",
                e.to_string(),
                event_id,
                effect.id
            )
            .execute(&self.pool)
            .await?;

            Err(e)
        }
    }
}
```

**Deliverable**: End-to-end event processing with durability

### Phase 3.5: Production Hardening (Critical!)

#### Issues That Will Break Production

##### 1. State Initialization Race

**Problem**: First event for a saga → `SELECT ... FOR UPDATE` returns zero rows → crash

**Fix**: Initialize state before loading (lines 429-436):
```rust
// Before step 3 in process_next_event:
sqlx::query!(
    "INSERT INTO seesaw_state (saga_id, state, version)
     VALUES ($1, $2, 1)
     ON CONFLICT (saga_id) DO NOTHING",
    queued.saga_id,
    Json(&S::default())
)
.execute(&mut *tx)
.await?;

// Now safe to SELECT FOR UPDATE
let (prev_state, version) = sqlx::query!(/* ... */)
```

##### 2. Effect Timeout

**Problem**: Effect hangs forever → worker deadlocked → system grinds to halt

**Fix**: Wrap effect execution with timeout (line 542):
```rust
// In run_effect_idempotent:
match tokio::time::timeout(
    Duration::from_secs(30),
    effect.handler.handle(event, ctx)
).await {
    Ok(Ok(result)) => {
        // Effect succeeded
        sqlx::query!(/* mark completed */).await?;
        Ok(result)
    }
    Ok(Err(e)) => {
        // Effect failed
        sqlx::query!(/* mark failed */).await?;
        Err(e)
    }
    Err(_) => {
        // Timeout - move to DLQ
        sqlx::query!(
            "INSERT INTO seesaw_dlq (event_id, effect_id, error, payload)
             VALUES ($1, $2, 'timeout', $3)",
            event_id, effect.id, Json(event)
        ).await?;
        Err(anyhow!("Effect timeout after 30s"))
    }
}
```

##### 3. Visibility Timeout Reaper (Events + Effects)

**Problem**: Worker crashes → events/effects locked forever → system deadlocks within hours

**Fix**: Background reaper task:
```rust
// Spawn reaper on engine start:
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        // Unlock stuck events
        sqlx::query!(
            "UPDATE seesaw_events
             SET locked_until = NULL, retry_count = retry_count + 1
             WHERE locked_until < NOW() - INTERVAL '5 minutes'
             AND processed_at IS NULL
             AND retry_count < 5"  // Max 5 retries
        )
        .execute(&pool)
        .await?;

        // Unlock stuck effect executions
        sqlx::query!(
            "UPDATE seesaw_effect_executions
             SET status = 'pending', attempts = attempts + 1
             WHERE status = 'executing'
             AND last_attempted_at < NOW() - INTERVAL '5 minutes'
             AND attempts < 3"
        )
        .execute(&pool)
        .await?;

        // Move permanently failed to DLQ
        sqlx::query!(
            "INSERT INTO seesaw_dlq (event_id, effect_id, error, payload)
             SELECT event_id, effect_id, error,
                    (SELECT payload FROM seesaw_events WHERE event_id = ee.event_id)
             FROM seesaw_effect_executions ee
             WHERE status = 'failed' AND attempts >= 3
             ON CONFLICT DO NOTHING"
        )
        .execute(&pool)
        .await?;

        // Record heartbeat (critical for monitoring)
        sqlx::query!(
            "INSERT INTO seesaw_reaper_heartbeat (last_run, events_reaped, effects_reaped)
             VALUES (NOW(),
                     (SELECT COUNT(*) FROM seesaw_events WHERE retry_count > 0),
                     (SELECT COUNT(*) FROM seesaw_dlq))
             ON CONFLICT (id) DO UPDATE
             SET last_run = NOW(),
                 events_reaped = EXCLUDED.events_reaped,
                 effects_reaped = EXCLUDED.effects_reaped"
        )
        .execute(&pool)
        .await?;
    }
});
```

##### 3.5. Reaper Heartbeat Monitor (Critical for Production)

**Problem**: Reaper hangs → zombies accumulate → queue depth hits 100k → system dies silently

**Solution**: Heartbeat table + alert on stale heartbeat

**Schema**:
```sql
CREATE TABLE seesaw_reaper_heartbeat (
    id INT PRIMARY KEY DEFAULT 1,  -- Single row table
    last_run TIMESTAMPTZ NOT NULL,
    events_reaped INT NOT NULL DEFAULT 0,
    effects_reaped INT NOT NULL DEFAULT 0,
    CHECK (id = 1)  -- Enforce single row
);

CREATE INDEX idx_reaper_heartbeat_last_run ON seesaw_reaper_heartbeat(last_run);
```

**Alert Query** (run every 5 minutes via monitoring):
```sql
-- Alert if reaper hasn't run in 3 minutes (2× expected interval)
SELECT
    EXTRACT(EPOCH FROM (NOW() - last_run)) as seconds_since_last_run,
    events_reaped,
    effects_reaped
FROM seesaw_reaper_heartbeat
WHERE last_run < NOW() - INTERVAL '3 minutes';
```

**Grafana/Datadog Alert**:
```yaml
alert: ReaperDead
expr: time() - seesaw_reaper_last_run_seconds > 180  # 3 minutes
severity: critical
message: |
  Seesaw Reaper has not run in {{ $value }}s.
  Zombie events/effects are accumulating.
  Check reaper process health immediately.
```

**Health Check Endpoint** (for load balancer):
```rust
// GET /health/reaper
pub async fn reaper_health_check(pool: &PgPool) -> Result<StatusCode> {
    let heartbeat = sqlx::query!(
        "SELECT last_run FROM seesaw_reaper_heartbeat"
    )
    .fetch_one(pool)
    .await?;

    let elapsed = Utc::now() - heartbeat.last_run;

    if elapsed.num_seconds() > 180 {
        return Err(StatusCode::SERVICE_UNAVAILABLE);  // 503
    }

    Ok(StatusCode::OK)  // 200
}
```

**Invariant**: If reaper misses 2 consecutive runs (3 minutes), the system is unhealthy. Alert immediately.

---

##### 4. Transactional Effect Intents (Gemini's Fix)

**Problem**: State committed → crash → effects never recorded → data loss

**Current approach** (lines 463-489):
```rust
tx.commit().await?;  // State saved
// Then run effects (outside transaction)
for effect in effects { /* ... */ }
```

**Better approach** - Insert effect execution records atomically with state:
```rust
// Step 6.5: Record effect intents in same transaction (with execution properties)
for effect in self.effects.handlers_for(&queued.event_type) {
    sqlx::query!(
        "INSERT INTO seesaw_effect_executions (
            event_id, effect_id, status,
            execute_at, timeout_seconds, max_attempts, priority
         )
         VALUES ($1, $2, 'pending', $3, $4, $5, $6)
         ON CONFLICT DO NOTHING",
        queued.event_id,
        effect.id,
        Utc::now() + effect.config.delay,  // ← From .delayed()
        effect.config.timeout.as_secs(),    // ← From .timeout()
        effect.config.max_attempts,         // ← From .retry()
        effect.config.priority              // ← From .priority()
    )
    .execute(&mut *tx)
    .await?;
}

// Step 7: Commit transaction (state + effect intents atomic)
tx.commit().await?;

// Step 8: Workers poll for ready effects (separate process)
// This happens in a background worker loop, not in this event handler
```

**Why better**:
- If crash happens after commit, effect records exist and will be processed when `execute_at` arrives
- Delayed effects persist across restarts
- Each effect has its own timeout/retry configuration

##### 5. Hot Saga Bottleneck

**Problem**: `FOR UPDATE` lock serializes all events for same saga → throughput collapses under load

**Impact**: Cannot scale beyond ~10 events/sec per saga

**Solutions** (pick one):

**Option A: Document limitation** (easiest)
```markdown
## Known Limitations

**Per-Saga Throughput**: ~10 events/sec per saga due to serialization lock.

If you have high-throughput sagas:
- Partition work across multiple saga_ids
- Use saga_id as sharding key
```

**Option B: Optimistic locking** (remove FOR UPDATE)
```rust
// Load without lock
let (prev_state, version) = sqlx::query!(
    "SELECT state, version FROM seesaw_state WHERE saga_id = $1",
    queued.saga_id
)
.fetch_one(&mut *tx)
.await?;

// Save with version check (will fail if concurrent update)
let rows_affected = sqlx::query!(
    "UPDATE seesaw_state
     SET state = $1, version = $2
     WHERE saga_id = $3 AND version = $4",
    Json(&next_state), version + 1, queued.saga_id, version
)
.execute(&mut *tx)
.await?
.rows_affected();

if rows_affected == 0 {
    // Version conflict - retry event
    tx.rollback().await?;
    self.queue.requeue(queued.id).await?;  // Push back to queue
    return Ok(());
}
```

**Trade-off**: More retries under contention, but no serialization lock.

**Recommendation**: Start with Option A (document), add Option B if needed.

##### 6. Graceful Shutdown

**Problem**: Deploy kills workers → events abandoned → data loss

**Fix**: Drain workers before exit:
```rust
// In main.rs:
let shutdown = Arc::new(AtomicBool::new(false));
let shutdown_clone = shutdown.clone();

tokio::spawn(async move {
    tokio::signal::ctrl_c().await.ok();
    info!("Received shutdown signal, draining workers...");
    shutdown_clone.store(true, Ordering::SeqCst);
});

// In worker loop:
loop {
    if shutdown.load(Ordering::SeqCst) {
        info!("Worker shutting down gracefully");
        break;  // Finish current event, then exit
    }

    self.process_next_event().await?;
}

// Wait for all workers to finish
engine.wait_for_workers().await?;
info!("All workers drained, safe to exit");
```

##### 7. Connection Pool Sizing

**Problem**: Long-running effects hold connections → pool exhaustion → system hangs

**Fix**: Size pool to handle concurrent workers + effects:
```rust
// Config:
let pool = PgPoolOptions::new()
    .max_connections(workers * 2)  // 2 connections per worker
    .acquire_timeout(Duration::from_secs(5))  // Fail fast
    .connect(&database_url)
    .await?;
```

Or use separate pool for effects:
```rust
struct EffectContext {
    effect_pool: PgPool,  // Separate pool, won't block event processing
    // ...
}
```

##### 8. idempotency_key Format (Gemini's Detail)

**Problem**: External APIs (Stripe) need string idempotency keys

**Fix**: Generate UUID v5 deterministically with custom namespace:
```rust
use uuid::Uuid;

// Define custom namespace (prevents collisions with other systems)
pub const NAMESPACE_SEESAW: Uuid = Uuid::from_bytes([
    0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1,
    0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);

// In EffectContext construction:
let idempotency_key = Uuid::new_v5(
    &NAMESPACE_SEESAW,  // ← Custom namespace, not NAMESPACE_OID
    format!("{}-{}", queued.event_id, effect.id).as_bytes()
).to_string();

// Now safe for Stripe:
ctx.deps().stripe.charge(
    amount,
    idempotency_key: &ctx.idempotency_key  // UUID string
).await?;
```

**Why custom namespace**: Avoids collisions with other systems using UUID v5 with OID namespace.

##### 9. Infinite Loop Protection

**Problem**: Bug in reducer/effect creates Event A → Effect → Event A loop → fills disk

**Fix**: Add `hops` count to envelope, move to DLQ after threshold:

```rust
// In event worker (Phase 1):
let event = queue.poll_next().await?;

// Check hop count
if event.hops >= 50 {
    sqlx::query!(
        "INSERT INTO seesaw_dlq (event_id, effect_id, error, payload, reason)
         VALUES ($1, 'infinite_loop', 'Event exceeded 50 hops', $2, 'infinite_loop')",
        event.event_id,
        Json(&event.payload)
    )
    .execute(&pool)
    .await?;

    // Mark event as processed (don't requeue)
    queue.ack(event.id).await?;
    return Ok(());
}

// Process normally...

// When effect returns new event, increment hops:
let next_event = effect.handler.handle(&event, &ctx).await?;
queue.publish_with_hops(next_event, event.hops + 1).await?;
```

##### 10. Dead Letter Queue (DLQ) Schema

**Add to Phase 2 schema**:
```sql
-- Dead letter queue for permanently failed effects AND infinite loops
CREATE TABLE seesaw_dlq (
    id BIGSERIAL PRIMARY KEY,
    event_id UUID NOT NULL,
    effect_id VARCHAR(255) NOT NULL,
    error TEXT NOT NULL,
    payload JSONB NOT NULL,
    reason VARCHAR(50) NOT NULL,     -- 'failed', 'timeout', 'infinite_loop'
    attempts INT NOT NULL DEFAULT 0,
    failed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ
);

CREATE INDEX idx_dlq_unresolved ON seesaw_dlq(resolved_at) WHERE resolved_at IS NULL;
CREATE INDEX idx_dlq_reason ON seesaw_dlq(reason) WHERE resolved_at IS NULL;
```

##### 10. Priority Queue (Gemini's Nice-to-Have)

**Add to Phase 1 schema**:
```sql
ALTER TABLE seesaw_events ADD COLUMN priority INT NOT NULL DEFAULT 10;

-- Update polling index to respect priority
DROP INDEX idx_events_pending;
CREATE INDEX idx_events_pending_priority
ON seesaw_events(priority DESC, created_at ASC)
WHERE processed_at IS NULL;

-- Update poll_next query:
UPDATE seesaw_events
SET locked_until = NOW() + INTERVAL '5 minutes'
WHERE id = (
    SELECT id FROM seesaw_events
    WHERE processed_at IS NULL
    AND (locked_until IS NULL OR locked_until < NOW())
    ORDER BY priority DESC, created_at ASC  -- ← Priority first
    LIMIT 1
    FOR UPDATE SKIP LOCKED
)
RETURNING *;
```

**Usage**:
```rust
engine.publish_with_priority(SystemReset { ... }, priority: 1).await?;  // Jump to front
```

##### 11. State Size Wall (Gemini's Gotcha)

**Problem**: Loading multi-megabyte JSONB state on every effect → IO spike → system slowdown

**Symptoms**:
- Effect latency increases as saga progresses
- Database CPU spikes
- Query time > 100ms for state load

**Fix**: Keep state lean, store large blobs externally

**Bad** (state bloat):
```rust
struct OrderState {
    order_id: Uuid,
    customer_name: String,
    invoice_pdf: Vec<u8>,      // ❌ 2MB PDF in state
    email_html: String,         // ❌ 50KB HTML in state
    product_images: Vec<Vec<u8>>, // ❌ 5MB of images
}
```

**Good** (external storage):
```rust
struct OrderState {
    order_id: Uuid,
    customer_name: String,
    invoice_url: String,        // ✅ S3 reference only
    email_template_id: Uuid,    // ✅ Just the ID
    image_urls: Vec<String>,    // ✅ CDN URLs
}

// Load blobs on-demand in effects
effect::on::<GenerateInvoice>()
    .id("send_invoice")
    .then(|event, ctx| async move {
        let state = ctx.state();

        // Fetch blob only when needed
        let pdf = ctx.deps().s3.get_object(
            "invoices",
            &format!("{}.pdf", state.invoice_url)
        ).await?;

        ctx.deps().mailer.send_with_attachment(pdf).await?;
        Ok(())
    });
```

**Rule of thumb**: Keep `seesaw_state` under **10KB per saga**
- Under 1KB: Excellent (IDs, enums, small strings)
- 1-10KB: Good (reasonable domain data)
- 10-100KB: Warning (consider slimming)
- 100KB+: Critical (will impact performance at scale)

**Monitoring**:
```sql
-- Alert on large states
SELECT saga_id,
       LENGTH(state::text) as state_bytes,
       state::jsonb -> 'order_id' as order_id
FROM seesaw_state
WHERE LENGTH(state::text) > 10240  -- >10KB
ORDER BY state_bytes DESC
LIMIT 10;
```

**Invariant**: State is for **coordination data** (IDs, status, counters), not **content blobs** (PDFs, images, large text).

#### Updated Estimates

**Additional LOC**: ~200 LOC (production hardening)
**Total LOC**: ~700 LOC (was 500)
**Additional Time**: 1 week
**Total Time**: 4 weeks

**Deliverable**: Production-grade queue-backed engine with:
- ✅ No worker deadlocks (timeouts)
- ✅ No stuck events (reaper)
- ✅ No data loss (transactional effect intents)
- ✅ Graceful deploys (shutdown handler)
- ✅ Scalability documented (hot saga limitation)
- ✅ DLQ for failed events
- ✅ Priority queue support

### Total Estimate

**Lines of Code**: ~700 LOC (was ~500)
**Time**: 4 weeks (was 3 weeks)
**Dependencies**: sqlx, tokio, serde, anyhow, uuid

## Usage Examples

### Basic Setup (Postgres)

```rust
use seesaw::{Engine, PostgresQueue, PostgresStateStore};
use sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<()> {
    let pool = PgPool::connect(&env::var("DATABASE_URL")?).await?;

    // Create engine with effects and reducers
    let engine = Engine::new(deps)
        .with_effect(
            effect::on::<OrderPlaced>()
                .id("process_order")  // ← REQUIRED for idempotency
                .then(|event, ctx| async move {
                    process_order(event, ctx).await?;
                    Ok(ApprovalRequested {
                        order_id: event.order_id,
                    })
                })
        )
        .with_effect(
            effect::on::<ApprovalRequested>()
                .id("send_approval_email")  // ← REQUIRED
                .then(|event, ctx| async move {
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

    // Create queue, state store, and runtime
    let queue = Arc::new(PostgresQueue::new(pool.clone()));
    let state = Arc::new(PostgresStateStore::new(pool));
    let engine = engine
        .with_queue(queue.clone())
        .with_state(state);

    let mut runtime = Runtime::new(engine.clone(), queue);
    runtime.spawn_workers(
        2,   // Event workers (fast - state updates)
        20,  // Effect workers (slow - IO-bound)
    );

    // Process initial event - framework generates saga_id
    engine.process(OrderPlaced {
        order_id: 123,
        // No saga_id! Envelope carries it ✅
    });

    info!("Started saga");

    // Keep running until shutdown signal
    tokio::signal::ctrl_c().await?;
    runtime.shutdown().await?;
    Ok(())
}
```

### Custom Backend Setup (NATS + Redis)

```rust
use seesaw::{Engine, Queue, StateStore};
use my_adapters::{NatsQueue, RedisStateStore};

#[tokio::main]
async fn main() -> Result<()> {
    // Use your existing NATS infrastructure
    let nc = async_nats::connect("nats://localhost:4222").await?;
    let queue = Arc::new(NatsQueue::new(nc));

    // Use your existing Redis cluster
    let redis = redis::Client::open("redis://localhost:6379")?;
    let state = Arc::new(RedisStateStore::new(redis));

    // Same engine code - backend swapped
    let engine = Engine::new(deps)
        .with_queue(queue.clone())
        .with_state(state);

    let mut runtime = Runtime::new(engine.clone(), queue);
    runtime.spawn_workers(2, 20);

    // Same API surface
    engine.process(OrderPlaced { order_id: 123 });

    tokio::signal::ctrl_c().await?;
    runtime.shutdown().await?;
    Ok(())
}
```

### Multi-Day Workflow (Delayed Effects)

```rust
// Day 1: Order placed - NEW saga
let saga_id = engine.process(OrderPlaced {
    order_id: Uuid::new_v4(),
    customer_email: "user@example.com",
    // No saga_id in event! ✅
}).await?;

// Store saga_id for later (e.g., in your orders table)
db.query!(
    "UPDATE orders SET saga_id = $1 WHERE order_id = $2",
    saga_id, order_id
).await?;

// Effect 1: Send immediate confirmation
effect::on::<OrderPlaced>()
    .id("send_confirmation")
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_confirmation(&event).await?;
        Ok(ConfirmationSent { order_id: event.order_id })
    });

// Effect 2: Send approval request after 2 days ⏰
effect::on::<OrderPlaced>()
    .id("send_approval_request")
    .delayed(Duration::from_days(2))  // ← Execute in 2 days
    .retry(5)                          // ← Retry 5 times if fails
    .timeout(Duration::from_secs(30))  // ← Max 30s execution
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_approval_request(&event).await?;
        Ok(ApprovalRequested { order_id: event.order_id })
    });

// Day 3: Approval received via webhook - CONTINUE existing saga
async fn handle_approval_webhook(order_id: Uuid) -> Result<()> {
    // Lookup saga_id from your database
    let saga_id = db.query!(
        "SELECT saga_id FROM orders WHERE order_id = $1",
        order_id
    ).fetch_one().await?.saga_id;

    // Process event in existing saga
    engine.process_saga(saga_id, || async move {
        Ok(ApprovalReceived {
            order_id,
            // No saga_id in event! ✅
        })
    }).await?;

    Ok(())
}

// Effect finalizes order
effect::on::<ApprovalReceived>()
    .id("finalize_order")
    .then(|event, ctx| async move {
        // ctx.state() loads LATEST state (accumulated since Day 1)
        finalize_order(event.order_id, ctx.state()).await?;

        Ok(OrderCompleted {
            order_id: event.order_id,
            // No saga_id! ✅
        })
    });
```

**Key**:
- Events persist in queue between Day 1 and Day 3
- saga_id is envelope metadata, not in user events
- Effects automatically inherit saga_id from context
- State is durable and isolated per saga_id

### External API Integration & Execution Properties

```rust
// Critical payment - long timeout, many retries
effect::on::<OrderPlaced>()
    .id("charge_payment")
    .timeout(Duration::from_secs(60))  // Stripe can be slow
    .retry(5)                           // Critical - retry more
    .priority(1)                        // High priority
    .then(|event, ctx| async move {
        let charge = ctx.deps().stripe.charge(
            event.amount,
            idempotency_key: &ctx.idempotency_key  // ← UUID v5(event_id + effect_id)
        ).await?;

        Ok(PaymentCharged {
            order_id: event.order_id,
            charge_id: charge.id,
        })
    });

// Send reminder 7 days after order
effect::on::<OrderPlaced>()
    .id("send_reminder")
    .delayed(Duration::from_days(7))
    .retry(3)
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_reminder(&event).await?;
        Ok(ReminderSent { order_id: event.order_id })
    });

// Fast logging - short timeout, no retries needed
effect::on::<OrderPlaced>()
    .id("log_order")
    .timeout(Duration::from_secs(5))
    .retry(1)  // Don't retry logs
    .then(|event, ctx| async move {
        tracing::info!(
            saga_id = %ctx.saga_id,
            event_id = %ctx.event_id,
            order_id = %event.order_id,
            "Processing order"
        );
        Ok(())
    });
```

### CQRS Pattern with Wait (Read-After-Write Consistency)

**Problem**: Queue-backed architecture is eventually consistent. How do you ensure state is updated before querying?

**Solution**: Use `.wait()` to block until terminal event is emitted (state guaranteed committed).

```rust
// ❌ Eventual consistency - state might not be ready yet
engine.process(CreateOrder { user_id: 123 });
let order = db.query_order(order_id).await?;  // Might return None!

// ✅ Wait for terminal event - state guaranteed updated
let order = engine
    .process(CreateOrder { user_id: 123 })
    .wait::<OrderCreated>()
    .timeout(Duration::from_secs(30))
    .await?;

// ✅ Handle success or failure
use seesaw::matches;
let result = engine
    .process(CreateOrder { user_id: 123 })
    .wait(matches! {
        OrderCreated(order) => Ok(order),
        OrderFailed(err) => Err(anyhow!("Order creation failed: {}", err.reason)),
        PaymentDeclined(decline) => Err(anyhow!("Payment declined: {}", decline.reason)),
    })
    .await?;

// ✅ Builder pattern for multiple terminal events
let result = engine
    .process(CreateOrder { user_id: 123 })
    .wait_any()
    .on::<OrderCreated>(|order| Ok(order))
    .on::<OrderFailed>(|err| Err(anyhow!("Failed: {}", err.reason)))
    .on::<PaymentDeclined>(|decline| Err(anyhow!("Declined: {}", decline.reason)))
    .timeout(Duration::from_secs(30))
    .await?;
```

**How It Works**:
1. `.wait()` subscribes to `LISTEN seesaw_saga_{saga_id}` **before** publishing
2. `process()` publishes event to queue
3. Workers process event → reducers update state → effects run → terminal event emitted
4. Trigger fires `NOTIFY seesaw_saga_{saga_id}` with event payload
5. `.wait()` receives notification, returns typed event
6. ✅ State is guaranteed committed (terminal event = workflow done)

**Performance**: Push-based (LISTEN/NOTIFY), not polling. Zero overhead when not using `.wait()`.

**Works with Both Inline and Queued Effects**:
- **Inline effects**: Terminal event emitted immediately during event processing
- **Queued effects**: Terminal event emitted when effect worker completes execution
- `.wait()` doesn't care - it just waits for ANY matching event on the saga

```rust
// Example: Wait for queued effect terminal event
effect::on::<OrderPlaced>()
    .id("charge_payment")
    .retry(5)  // ← Queued (might take time, retries)
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    });

// Wait for PaymentCharged (even though it's from queued effect)
let payment = engine
    .process(OrderPlaced { ... })
    .wait::<PaymentCharged>()  // ← Works! Waits for queued effect to complete
    .timeout(Duration::from_secs(60))
    .await?;
```

### Priority Handling

**Decision**: Priority is set on **effects**, not on external events.

**Rationale**:
- External events (webhooks, user actions) don't have inherent priority
- Priority matters for **side effects** (e.g., urgent emails vs. background cleanup)
- Queue workers poll effects by priority, not events

```rust
// High priority effect (jump to front of effect queue)
effect::on::<SystemAlert>()
    .id("send_urgent_alert")
    .priority(1)  // ← Priority set on effect, not engine.process()
    .then(|event, ctx| async move {
        ctx.deps().pagerduty.alert(&event).await?;
        Ok(())
    });

// Normal priority effect
effect::on::<UserSignedUp>()
    .id("send_welcome_email")
    .priority(10)  // ← Default priority
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_welcome(&event).await?;
        Ok(())
    });

// Low priority effect (background work)
effect::on::<DataExport>()
    .id("export_csv")
    .priority(20)
    .then(|event, ctx| async move {
        ctx.deps().exporter.generate_csv(&event).await?;
        Ok(())
    });
```

**Worker Behavior**:
```sql
-- Effect worker polls by priority DESC
SELECT * FROM seesaw_effect_executions
WHERE status = 'pending' AND execute_at <= NOW()
ORDER BY priority DESC, execute_at ASC  -- ← High priority first
LIMIT 1
FOR UPDATE SKIP LOCKED;
```

### Graceful Shutdown

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let pool = PgPool::connect(&env::var("DATABASE_URL")?).await?;

    let engine = Engine::new(deps)
        .with_effect(/* ... */)
        .with_reducer(/* ... */);

    let queue = PostgresQueue::new(pool)
        .start_workers(4, engine.clone())
        .await?;

    let engine = engine.with_queue(queue);

    // Handle shutdown signal
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down...");
            engine.shutdown().await?;  // Signals workers to drain
            info!("All workers stopped");
        }
    }

    Ok(())
}
```

### Dead Letter Queue Management

```rust
// List permanently failed effects:
let failed = engine.list_dlq().await?;
for entry in failed {
    println!("Failed: {} - {} (attempts: {})",
        entry.event_id, entry.effect_id, entry.attempts);
}

// Retry a failed effect:
engine.retry_from_dlq(event_id, "charge_payment").await?;

// Mark as resolved (won't retry):
engine.resolve_dlq(event_id, "charge_payment").await?;
```

## API Summary

### Breaking Changes from v0.7 (Stateless)

| v0.7 Stateless API | Queue-Backed API | Notes |
|-------------------|------------------|-------|
| `engine.activate(state)` | `runtime.spawn_workers(N, M)` | Runtime owns workers, not queue |
| `handle.run(\|_\| Ok(Event))` | `engine.process(event)` | Fire-and-forget (no await) |
| `handle.settled().await` | `engine.process(e).wait::<E>().await` | Wait for terminal event via LISTEN/NOTIFY |
| N/A | `engine.shutdown().await` | Graceful worker drain |
| `effect::on::<E>().then(...)` | `effect::on::<E>().id("name").then(...)` | **ID required** for idempotency |
| Event has saga_id field | **saga_id in envelope** | Events are pure business data |
| N/A | `ctx.saga_id`, `ctx.event_id` | Context exposes envelope metadata |
| N/A | `ctx.idempotency_key` | UUID v5 for external API idempotency |

### Runtime Methods (Owns Workers)

```rust
pub struct Runtime<S, D, Q: Queue, St: StateStore> {
    engine: Arc<Engine<S, D, Q, St>>,
    queue: Arc<Q>,
    event_workers: JoinSet<()>,
    effect_workers: JoinSet<()>,
}

impl<S, D, Q: Queue, St: StateStore> Runtime<S, D, Q, St> {
    pub fn new(engine: Arc<Engine<S, D, Q, St>>, queue: Arc<Q>) -> Self;

    pub fn spawn_workers(&mut self, event_count: usize, effect_count: usize);

    pub fn engine(&self) -> &Arc<Engine<S, D, Q, St>>;

    pub async fn shutdown(self) -> Result<()>;
}
```

### Queue Trait (Implement Your Own)

```rust
#[async_trait]
pub trait Queue: Send + Sync + 'static {
    async fn publish(&self, envelope: EventEnvelope) -> Result<()>;
    async fn poll_event(&self) -> Result<Option<EventEnvelope>>;
    async fn poll_effect(&self) -> Result<Option<EffectIntent>>;
    async fn ack_event(&self, event_id: Uuid) -> Result<()>;
    async fn ack_effect(&self, event_id: Uuid, effect_id: &str) -> Result<()>;
}

#[async_trait]
pub trait StateStore: Send + Sync + 'static {
    async fn load<S: DeserializeOwned + Send>(&self, saga_id: Uuid) -> Result<Option<S>>;
    async fn save<S: Serialize + Send>(&self, saga_id: Uuid, state: &S, version: i32) -> Result<()>;
}
```

### PostgresQueue Implementation (Ships with Seesaw)

```rust
pub struct PostgresQueue {
    pool: PgPool,
}

impl PostgresQueue {
    pub fn new(pool: PgPool) -> Self;
}

#[async_trait]
impl Queue for PostgresQueue {
    async fn publish(&self, envelope: EventEnvelope) -> Result<()>;
    async fn poll_event(&self) -> Result<Option<EventEnvelope>>;
    async fn poll_effect(&self) -> Result<Option<EffectIntent>>;
    async fn ack_event(&self, event_id: Uuid) -> Result<()>;
    async fn ack_effect(&self, event_id: Uuid, effect_id: &str) -> Result<()>;
}

pub struct PostgresStateStore {
    pool: PgPool,
}

impl PostgresStateStore {
    pub fn new(pool: PgPool) -> Self;
}

#[async_trait]
impl StateStore for PostgresStateStore {
    async fn load<S: DeserializeOwned + Send>(&self, saga_id: Uuid) -> Result<Option<S>>;
    async fn save<S: Serialize + Send>(&self, saga_id: Uuid, state: &S, version: i32) -> Result<()>;
}
```

### Engine Methods

```rust
pub struct Engine<S, D, Q: Queue, St: StateStore> {
    queue: Arc<Q>,
    state_store: Arc<St>,
    deps: Arc<D>,
    // ...
}

impl<S, D, Q: Queue, St: StateStore> Engine<S, D, Q, St> {
    // Setup
    pub fn new(deps: D) -> Self;
    pub fn with_deps(deps: D) -> Self;  // Alias for clarity
    pub fn with_queue(self, queue: Arc<Q>) -> Self;
    pub fn with_state(self, state: Arc<St>) -> Self;

    // Process external events (entry points)
    pub fn process(&self, event: impl Event) -> ProcessHandle;  // Returns handle for wait/fire-and-forget

    // Lifecycle
    pub async fn shutdown(&self) -> Result<()>;

    // Internal: called by workers (not public API)
    async fn process_event(&self, envelope: EventEnvelope) -> Result<Vec<Event>>;
}

// ProcessHandle - enables wait or fire-and-forget patterns
pub struct ProcessHandle {
    queue: Arc<PostgresQueue>,
    saga_id: Uuid,
    event: Box<dyn Event>,
}

impl ProcessHandle {
    // Wait for specific terminal event (type-safe)
    pub fn wait<E: Event>(self) -> WaitHandle<E>;

    // Wait for multiple terminal events with pattern matching
    pub fn wait<R>(self, matcher: impl EventMatcher<R>) -> WaitHandle<R>;

    // Wait for any of multiple event types (builder pattern)
    pub fn wait_any(self) -> WaitAnyHandle;
}

// Drop impl publishes event asynchronously (fire-and-forget)
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // Spawns background task to publish event
    }
}

// WaitHandle - awaitable future that publishes event and waits for terminal event
pub struct WaitHandle<E> {
    queue: Arc<PostgresQueue>,
    saga_id: Uuid,
    event: Box<dyn Event>,
    timeout: Option<Duration>,
}

impl<E: Event> WaitHandle<E> {
    pub fn timeout(self, duration: Duration) -> Self;
}

// Implements IntoFuture - publishes event, then waits via LISTEN/NOTIFY
impl<E: Event> IntoFuture for WaitHandle<E> {
    type Output = Result<E>;
    // ... LISTEN/NOTIFY implementation
}

// WaitAnyHandle - builder for matching multiple terminal events
pub struct WaitAnyHandle {
    queue: Arc<PostgresQueue>,
    saga_id: Uuid,
    event: Box<dyn Event>,
    matchers: Vec<Box<dyn EventMatcher>>,
    timeout: Option<Duration>,
}

impl WaitAnyHandle {
    pub fn on<E: Event, R>(self, handler: impl Fn(E) -> Result<R>) -> Self;
    pub fn timeout(self, duration: Duration) -> Self;
}

impl<R> IntoFuture for WaitAnyHandle {
    type Output = Result<R>;
    // Waits for first matching event, applies handler
}

// Trait for matching events (supports matches! macro)
pub trait EventMatcher<R> {
    fn try_match(&self, event: &dyn Event) -> Option<Result<R>>;
}

// Macro for ergonomic pattern matching
#[macro_export]
macro_rules! matches {
    ($($pattern:pat => $result:expr),+ $(,)?) => {
        // Generates EventMatcher implementation
    };
}
```

**Usage Patterns**:

```rust
// Fire-and-forget (no await, publishes on drop)
engine.process(OrderPlaced { ... });

// Wait for terminal event (CQRS-friendly)
let order = engine
    .process(OrderPlaced { ... })
    .wait::<OrderCreated>()
    .await?;

// With timeout
let order = engine
    .process(OrderPlaced { ... })
    .wait::<OrderCreated>()
    .timeout(Duration::from_secs(30))
    .await?;

// Handle multiple terminal events (success or failure)
use seesaw::matches;
let result = engine
    .process(OrderPlaced { ... })
    .wait(matches! {
        OrderCreated(order) => Ok(order),
        OrderFailed(err) => Err(anyhow!("Order failed: {}", err.reason)),
    })
    .await?;

// Alternative: wait_any with type-safe matching
let result = engine
    .process(OrderPlaced { ... })
    .wait_any()
    .on::<OrderCreated>(|order| Ok(order))
    .on::<OrderFailed>(|err| Err(anyhow!("Failed: {}", err.reason)))
    .await?;
```

### EffectBuilder Changes

```rust
// Type-state pattern enforces .id() at compile time
impl<E> EffectBuilder<E, NoId> {
    pub fn id(self, id: &str) -> EffectBuilder<E, HasIdTag>;  // ← Required
}

impl<E> EffectBuilder<E, HasIdTag> {
    // Execution mode (determines inline vs queued)
    pub fn queued(self) -> Self;                               // Force queued execution

    // Execution timing (automatically triggers queued)
    pub fn delayed(self, duration: Duration) -> Self;          // → Queued
    pub fn scheduled_at(self, time: DateTime<Utc>) -> Self;    // → Queued

    // Execution constraints (automatically triggers queued)
    pub fn timeout(self, duration: Duration) -> Self;          // → Queued (default: 30s)
    pub fn retry(self, attempts: u32) -> Self;                 // → Queued (default: 1 = inline)
    pub fn retry_policy(self, policy: RetryPolicy) -> Self;    // → Queued

    // Priority (automatically triggers queued)
    pub fn priority(self, priority: i32) -> Self;              // → Queued (default: 10)

    pub fn then<F>(self, handler: F) -> Effect<E>;  // ← Only available after .id()
}

// Examples:

// Inline (default) - zero overhead
effect::on::<Event>()
    .id("log_event")
    .then(|event, ctx| async move { Ok(()) });

// Queued (explicit) - persisted to seesaw_effect_executions
effect::on::<Event>()
    .id("long_running")
    .queued()
    .then(|event, ctx| async move { /* slow work */ Ok(()) });

// Queued (implicit via .delayed()) - scheduled execution
effect::on::<Event>()
    .id("send_reminder")
    .delayed(Duration::from_days(7))
    .then(|event, ctx| async move { Ok(()) });

// Queued (implicit via .retry()) - crash recovery
effect::on::<Event>()
    .id("charge_payment")
    .retry(5)
    .timeout(Duration::from_secs(60))
    .then(|event, ctx| async move { Ok(()) });
```

### `on!` Macro with Attributes

For enum events with multiple variants, use the `on!` macro with a single `#[effect(...)]` attribute:

```rust
use seesaw::on;

let effects = on! {
    // Simple effect - only id required
    #[effect(id = "enqueue_extract")]
    CrawlEvent::WebsiteIngested { website_id, job_id, .. } |
    CrawlEvent::WebsitePostsRegenerated { website_id, job_id, .. } => |ctx| async move {
        ctx.deps().jobs.enqueue(ExtractPostsJob { website_id }).await?;
        Ok(CrawlEvent::ExtractJobEnqueued { website_id })
    },

    // Effect with delays and retries - combined attribute
    #[effect(id = "send_reminder", delayed = 7days, retry = 5, timeout = 30)]
    CrawlEvent::PostsExtracted { website_id, .. } => |ctx| async move {
        ctx.deps().mailer.send_reminder(website_id).await?;
        Ok(CrawlEvent::ReminderSent { website_id })
    },

    // Critical effect - high priority
    #[effect(id = "sync_posts", priority = 1, timeout = 120, retry = 5)]
    CrawlEvent::PostsExtracted { website_id, posts, .. } => |ctx| async move {
        ctx.deps().sync(website_id, posts).await?;
        Ok(CrawlEvent::SyncComplete { website_id })
    },
};

// Returns Vec<Effect<S, D>> - add to engine
let engine = effects.into_iter().fold(Engine::new(deps), |e, eff| e.with_effect(eff));
```

#### Attribute Syntax

```rust
#[effect(
    id = "name",           // Required - effect identifier (string)
    delayed = 7days,       // Optional - delay execution (Ndays, Nhours, Nsecs)
    retry = 5,             // Optional - max attempts (integer, default: 3)
    timeout = 30,          // Optional - max seconds (integer, default: 30)
    priority = 1           // Optional - queue priority (integer, default: 10)
)]
```

#### Macro Expansion

```rust
// This:
#[effect(id = "send_reminder", delayed = 7days, retry = 5, timeout = 30)]
CrawlEvent::PostsExtracted { website_id, .. } => |ctx| async move { ... }

// Expands to:
effect::on::<CrawlEvent>()
    .extract(|e| match e {
        CrawlEvent::PostsExtracted { website_id, .. } => Some(website_id.clone()),
        _ => None,
    })
    .id("send_reminder")
    .delayed(Duration::from_days(7))
    .retry(5)
    .timeout(Duration::from_secs(30))
    .then(|website_id, ctx| async move { ... })
```

### EffectContext Changes

```rust
pub struct EffectContext<S, D> {
    // State access (loaded fresh from DB, immutable snapshot)
    state: Arc<S>,                // ← Latest state, safe to clone across .await

    // Dependencies
    pub deps: Arc<D>,

    // Envelope metadata (NEW)
    pub saga_id: Uuid,            // ← From envelope, not user event
    pub event_id: Uuid,           // ← Current event's unique ID
    pub idempotency_key: String,  // ← UUID v5(event_id + effect_id)
}

impl<S, D> EffectContext<S, D>
where
    S: Clone,
{
    /// Get current state (cheap Arc clone, safe across .await)
    ///
    /// Returns Arc<S> to avoid deadlock risk from holding RwLockReadGuard across .await.
    /// State is loaded fresh from DB by effect worker, so this is the latest state.
    pub fn state(&self) -> Arc<S> {
        self.state.clone()  // Arc::clone is cheap (just pointer + refcount)
    }

    /// Get dependencies
    pub fn deps(&self) -> &Arc<D> {
        &self.deps
    }
}

// Effects see latest state (not snapshot):
effect::on::<OrderPlaced>()
    .id("send_email")
    .then(|event, ctx| async move {
        let state = ctx.state();  // ← Fresh from DB
        ctx.deps().mailer.send(state.user_email).await?;

        // Event inherits ctx.saga_id automatically ✅
        Ok(EmailSent { order_id: event.order_id })
    });
```

**Removed**: `ctx.prev_state()` and `ctx.next_state()` - effects only see latest state.

### DLQ Management (Queue Responsibility, Not Engine)

**Decision**: DLQ is a **queue/worker concern**, not engine business logic.

```rust
// PostgresQueue handles DLQ, not Engine
impl PostgresQueue {
    /// List failed effects in DLQ
    pub async fn list_dlq(&self) -> Result<Vec<DlqEntry>> {
        sqlx::query_as!(
            DlqEntry,
            "SELECT event_id, effect_id, error, created_at, payload
             FROM seesaw_dlq
             ORDER BY created_at DESC
             LIMIT 100"
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Retry a failed effect from DLQ
    pub async fn retry_from_dlq(&self, event_id: Uuid, effect_id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // Move from DLQ back to effect_executions
        sqlx::query!(
            "INSERT INTO seesaw_effect_executions (event_id, effect_id, saga_id, status, attempts)
             SELECT event_id, effect_id, saga_id, 'pending', 0
             FROM seesaw_dlq
             WHERE event_id = $1 AND effect_id = $2",
            event_id,
            effect_id
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            "DELETE FROM seesaw_dlq WHERE event_id = $1 AND effect_id = $2",
            event_id,
            effect_id
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Mark DLQ entry as resolved (delete without retry)
    pub async fn resolve_dlq(&self, event_id: Uuid, effect_id: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM seesaw_dlq WHERE event_id = $1 AND effect_id = $2",
            event_id,
            effect_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// Usage (ops/admin code, not application code)
let queue = PostgresQueue::new(pool);

// List failed effects
let failed = queue.list_dlq().await?;
for entry in failed {
    println!("Failed: {:?} - {}", entry.effect_id, entry.error);
}

// Retry specific failure
queue.retry_from_dlq(event_id, "send_email").await?;
```

**Invariant**: Engine never touches DLQ. Workers move effects to DLQ on permanent failure (timeout, max retries).

---

### Saga Introspection APIs (Progress Tracking)

**Use Case**: Frontend progress indicators, admin dashboards, debugging

**Decision**: All data access belongs on **PostgresQueue** (store layer), not Engine.

```rust
impl PostgresQueue {
    // ============ Saga Progress Tracking ============

    /// Get current state for a saga
    pub async fn get_saga_state<S>(&self, saga_id: Uuid) -> Result<Option<S>>
    where
        S: serde::de::DeserializeOwned,
    {
        let row = sqlx::query!(
            "SELECT state FROM seesaw_state WHERE saga_id = $1",
            saga_id
        )
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(serde_json::from_value(r.state)?)),
            None => Ok(None),
        }
    }

    /// Get all effect executions for a saga (detailed view)
    pub async fn list_saga_effects(&self, saga_id: Uuid) -> Result<Vec<EffectExecution>> {
        sqlx::query_as!(
            EffectExecution,
            "SELECT event_id, effect_id, status, execute_at,
                    completed_at, attempts, error, event_type
             FROM seesaw_effect_executions
             WHERE saga_id = $1
             ORDER BY execute_at",
            saga_id
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Get saga summary (effect counts by status)
    pub async fn get_saga_summary(&self, saga_id: Uuid) -> Result<SagaSummary> {
        let row = sqlx::query!(
            "SELECT
                COUNT(*) FILTER (WHERE status = 'pending') as pending,
                COUNT(*) FILTER (WHERE status = 'executing') as executing,
                COUNT(*) FILTER (WHERE status = 'completed') as completed,
                COUNT(*) FILTER (WHERE status = 'failed') as failed
             FROM seesaw_effect_executions
             WHERE saga_id = $1",
            saga_id
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(SagaSummary {
            saga_id,
            pending: row.pending.unwrap_or(0) as u32,
            executing: row.executing.unwrap_or(0) as u32,
            completed: row.completed.unwrap_or(0) as u32,
            failed: row.failed.unwrap_or(0) as u32,
        })
    }

    /// List all active sagas (have pending/executing effects)
    pub async fn list_active_sagas(&self) -> Result<Vec<SagaInfo>> {
        sqlx::query_as!(
            SagaInfo,
            "SELECT DISTINCT saga_id,
                    MIN(execute_at) as started_at,
                    COUNT(*) as total_effects
             FROM seesaw_effect_executions
             WHERE status IN ('pending', 'executing')
             GROUP BY saga_id
             ORDER BY started_at DESC"
        )
        .fetch_all(&self.pool)
        .await
    }
}

// Supporting types
pub struct EffectExecution {
    pub event_id: Uuid,
    pub effect_id: String,
    pub status: String,  // 'pending', 'executing', 'completed', 'failed'
    pub execute_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub attempts: i32,
    pub error: Option<String>,
    pub event_type: String,
}

pub struct SagaSummary {
    pub saga_id: Uuid,
    pub pending: u32,
    pub executing: u32,
    pub completed: u32,
    pub failed: u32,
}

pub struct SagaInfo {
    pub saga_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub total_effects: i64,
}
```

**Usage Example: Crawl Progress API**

```rust
#[get("/crawl/{saga_id}/progress")]
async fn crawl_progress(
    saga_id: web::Path<Uuid>,
    queue: web::Data<Arc<PostgresQueue>>
) -> Result<Json<CrawlProgress>> {
    // Get effect summary (one efficient query)
    let summary = queue.get_saga_summary(*saga_id).await?;

    // Get domain state
    let state: CrawlState = queue.get_saga_state(*saga_id).await?
        .ok_or_else(|| anyhow!("Saga not found"))?;

    // Get detailed effects if needed
    let effects = queue.list_saga_effects(*saga_id).await?;

    Ok(Json(CrawlProgress {
        saga_id: *saga_id,
        status: state.status,
        pages_crawled: state.pages_crawled,
        pages_total: state.pages_total,
        current_phase: state.current_phase,
        effects_pending: summary.pending,
        effects_executing: summary.executing,
        effects_completed: summary.completed,
        effects_failed: summary.failed,
        effects: effects.iter().map(|e| EffectInfo {
            name: e.effect_id.clone(),
            status: e.status.clone(),
            error: e.error.clone(),
        }).collect(),
    }))
}

// Frontend polls this endpoint every 2s
// GET /crawl/550e8400-e29b-41d4-a716-446655440000/progress
// {
//   "saga_id": "550e8400-e29b-41d4-a716-446655440000",
//   "status": "Crawling",
//   "pages_crawled": 42,
//   "pages_total": 100,
//   "current_phase": "Extracting",
//   "effects_pending": 58,
//   "effects_executing": 3,
//   "effects_completed": 42,
//   "effects_failed": 0
// }
```

**Usage Example: Admin Dashboard**

```rust
#[get("/admin/sagas")]
async fn list_sagas(
    queue: web::Data<Arc<PostgresQueue>>
) -> Result<Json<Vec<SagaListItem>>> {
    let sagas = queue.list_active_sagas().await?;

    let mut items = vec![];
    for saga in sagas {
        if let Some(state) = queue.get_saga_state::<CrawlState>(saga.saga_id).await? {
            items.push(SagaListItem {
                saga_id: saga.saga_id,
                website_url: state.website_url,
                status: state.status,
                started_at: saga.started_at,
                total_effects: saga.total_effects,
            });
        }
    }

    Ok(Json(items))
}
```

**Rationale**:
- ✅ Common use case (every app needs progress tracking)
- ✅ Read-only (no state mutations)
- ✅ Efficient queries (uses indexes, aggregates)
- ✅ Type-safe (returns structs)
- ✅ Store concern (PostgresQueue owns DB access)

---

### Migration Checklist

- [ ] **Remove `saga_id` from all event structs** - now in envelope
- [ ] Create `Runtime` and spawn workers: `runtime.spawn_workers(event_count, effect_count)`
- [ ] Replace `handle.run(|_| Ok(Event))` with `engine.process(event)`
- [ ] For webhooks, use `engine.process_with_id(webhook_id, event)` for idempotency
- [ ] Replace `handle.settled()` with `engine.shutdown()`
- [ ] Add `.id("name")` to all effects (compile will enforce)
- [ ] Use `ctx.saga_id` instead of `event.saga_id`
- [ ] Use `ctx.idempotency_key` for external API calls (Stripe, etc.)
- [ ] Add database setup (migrations for queue tables)
- [ ] Configure connection pool sizing (`max_connections = workers * 2`)
- [ ] Add graceful shutdown handler (`ctrl-c` → `engine.shutdown()`)

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
**4 Workers**: ~40-400 events/sec (aggregate across all sagas)
**Bottleneck**: Usually your business logic, not the queue

**Per-Saga Limit**: ~10 events/sec per saga_id (due to FOR UPDATE serialization)
- Multiple sagas process in parallel
- Single hot saga serializes on state lock
- See Phase 3.5 #5 for mitigation strategies

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
- ✅ **Production-grade** - Timeouts, reapers, DLQ, graceful shutdown
- ✅ **Compile-time safety** - Idempotency enforced via type-state pattern

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

## Scaling to Millions (Future Considerations)

### Current Design: Good to ~100k users

**Architecture**: Postgres-backed queue, 2-20 workers

**Limitations at scale** (millions of users, 1000+ events/sec):

#### 1. Postgres Bloat
**Problem**: INSERT + UPDATE = table bloat at 1000+ events/sec
**The Math**:
- 1000 events/sec = **86.4 million events/day**
- Every event is created and then marked `processed_at` (soft delete)
- Postgres VACUUM must reclaim 86M rows/day
- **Without partitioning**: `DELETE FROM seesaw_events WHERE processed_at < NOW() - 30 days` will lock table for **hours**

**Solution** (Mandatory, Not Optional):

1. **Daily Table Partitioning** - Drop partition = instant metadata operation
```sql
-- Create partitioned table (one-time setup)
CREATE TABLE seesaw_events (
    id BIGSERIAL,
    event_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- ... other columns
) PARTITION BY RANGE (created_at);

-- Create today's partition (run daily via cron)
CREATE TABLE seesaw_events_2026_02_05 PARTITION OF seesaw_events
FOR VALUES FROM ('2026-02-05 00:00:00') TO ('2026-02-06 00:00:00');

-- Drop 30-day-old partitions (instant, no table lock)
DROP TABLE IF EXISTS seesaw_events_2026_01_06;
```

2. **Aggressive Autovacuum** (tune for high churn)
```sql
ALTER TABLE seesaw_events SET (
    autovacuum_vacuum_scale_factor = 0.01,  -- Vacuum at 1% dead tuples (not 20%)
    autovacuum_analyze_scale_factor = 0.005,
    autovacuum_vacuum_cost_limit = 10000     -- Faster vacuuming
);
```

3. **Archive Hot Partitions** (for debugging/compliance)
```sql
-- Weekly: archive processed events from yesterday's partition
INSERT INTO seesaw_events_archive
SELECT * FROM seesaw_events_2026_02_04
WHERE processed_at IS NOT NULL;

-- Then drop the partition
DROP TABLE seesaw_events_2026_02_04;
```

**Invariant**: At 1000+ eps, partitioning is **not optional**. Without it, VACUUM becomes the bottleneck within 2 weeks.

#### 2. Hot Saga Bottleneck
**Problem**: `FOR UPDATE` on single saga serializes all events for that user
**Solution**: Optimistic locking (Phase 3.5 #5 already documented)

```rust
// No FOR UPDATE - check version on save
let rows = sqlx::query!(
    "UPDATE seesaw_state SET state = $1, version = version + 1
     WHERE saga_id = $2 AND version = $3",
    state, saga_id, version
).execute(&tx).await?.rows_affected();

if rows == 0 {
    // Version conflict - requeue event
    queue.requeue(event.id).await?;
    return Ok(());
}
```

#### 3. Priority Starvation
**Problem**: Low-priority surge blocks critical events
**Solution**: Dedicated high-priority worker pool

```rust
// High-priority workers (only poll priority < 5)
let high_priority_workers = PostgresQueue::new(pool.clone())
    .with_priority_filter(0..5)
    .start_workers(5, 50, engine.clone())
    .await?;

// Normal workers (poll all priorities)
let normal_workers = PostgresQueue::new(pool)
    .start_workers(2, 20, engine.clone())
    .await?;
```

#### 4. Observer Effect Waste
**Problem**: Loading 10KB state for stateless observers (logging)
**Solution**: Mark effects as stateless

```rust
#[effect(id = "log", stateless = true)]  // ← Skip state loading
OrderPlaced { .. } => |ctx| async move {
    tracing::info!("Order placed");
    Ok(())  // No state needed
}
```

#### 5. Connection Pool Exhaustion
**Problem**: 20 effect workers holding connections during slow IO
**Solution**: Throttled effect worker (see below)

#### 6. Data Plane vs Control Plane
**Architecture split at scale**:
- **Postgres Seesaw**: Source of truth (orders, approvals, account changes)
- **NATS/Kafka**: High-volume telemetry (logs, analytics, likes, views)

```rust
// Control plane (durable, transactional)
engine.process(OrderPlaced { .. }).await?;  // → Postgres

// Data plane (high-volume, lossy OK)
nats.publish("metrics.page_view", PageView { .. }).await?;  // → NATS
```

### Throttled Effect Worker (Connection Pool Protection)

**Problem**: 20 effect workers × 60s timeout = 20 connections held for minutes → pool exhausted

**Solution**: Acquire/release connections per effect, use semaphore for concurrency limit

```rust
use tokio::sync::Semaphore;

struct ThrottledEffectWorker {
    pool: PgPool,
    semaphore: Arc<Semaphore>,  // Limit concurrent effects
    engine: Arc<Engine>,
}

impl ThrottledEffectWorker {
    fn new(pool: PgPool, max_concurrent: usize, engine: Arc<Engine>) -> Self {
        Self {
            pool,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            engine,
        }
    }

    async fn run(&self) -> Result<()> {
        loop {
            // 1. Acquire connection briefly to poll
            let pending = {
                let mut conn = self.pool.acquire().await?;

                sqlx::query_as!(
                    PendingEffect,
                    "UPDATE seesaw_effect_executions
                     SET status = 'executing', claimed_at = NOW()
                     WHERE (event_id, effect_id) = (
                         SELECT event_id, effect_id
                         FROM seesaw_effect_executions
                         WHERE status = 'pending'
                         AND execute_at <= NOW()
                         ORDER BY priority DESC, execute_at ASC
                         LIMIT 1
                         FOR UPDATE SKIP LOCKED
                     )
                     RETURNING *"
                )
                .fetch_optional(&mut *conn)
                .await?
                // Connection released here! ✅
            };

            let Some(pending) = pending else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };

            // 2. Acquire semaphore permit (limits concurrent effects)
            let permit = self.semaphore.clone().acquire_owned().await?;

            // 3. Spawn effect execution (doesn't hold connection during IO)
            let pool = self.pool.clone();
            let engine = self.engine.clone();

            tokio::spawn(async move {
                let _permit = permit;  // Hold permit for duration

                // Execute effect (connection acquired only when needed)
                match Self::execute_effect(&pool, &engine, &pending).await {
                    Ok(Some(next_event)) => {
                        // Acquire connection briefly to publish
                        let mut conn = pool.acquire().await?;
                        Self::publish_event(&mut conn, next_event, pending.hops + 1).await?;
                        Self::mark_completed(&mut conn, &pending).await?;
                        // Connection released ✅
                    }
                    Ok(None) => {
                        let mut conn = pool.acquire().await?;
                        Self::mark_completed(&mut conn, &pending).await?;
                    }
                    Err(e) => {
                        let mut conn = pool.acquire().await?;
                        Self::mark_failed(&mut conn, &pending, &e).await?;
                    }
                }

                Ok::<_, anyhow::Error>(())
            });
        }
    }

    async fn execute_effect(
        pool: &PgPool,
        engine: &Engine,
        pending: &PendingEffect
    ) -> Result<Option<Event>> {
        // Load state (acquire connection briefly)
        let (state, event_payload) = {
            let mut conn = pool.acquire().await?;
            let state = sqlx::query!(
                "SELECT state FROM seesaw_state WHERE saga_id = $1",
                pending.saga_id
            )
            .fetch_one(&mut *conn)
            .await?;

            let event = sqlx::query!(
                "SELECT payload FROM seesaw_events WHERE event_id = $1",
                pending.event_id
            )
            .fetch_one(&mut *conn)
            .await?;

            (state.state, event.payload)
            // Connection released! ✅
        };

        // Build context
        let ctx = EffectContext {
            state,
            saga_id: pending.saga_id,
            event_id: pending.event_id,
            idempotency_key: /* ... */,
            deps: engine.deps.clone(),
        };

        // Execute effect (NO CONNECTION HELD during external API calls!)
        let effect = engine.effects.get(&pending.effect_id);
        let result = tokio::time::timeout(
            Duration::from_secs(pending.timeout_seconds as u64),
            effect.handler.handle(&event_payload, &ctx)
        ).await??;

        Ok(result)
    }
}
```

**Key improvements**:
- ✅ Connections acquired only when needed (poll, load, save)
- ✅ Connections released during slow IO (Stripe API call)
- ✅ Semaphore limits concurrent effects (protects pool)
- ✅ Can run 100 workers with 10-connection pool

**Configuration**:
```rust
let pool = PgPoolOptions::new()
    .max_connections(20)  // Small pool!
    .acquire_timeout(Duration::from_secs(5))
    .connect(&database_url)
    .await?;

// 50 workers sharing 20 connections via semaphore
let worker = ThrottledEffectWorker::new(pool, 50, engine);
```

---

## Production Readiness Checklist

### 1. Monitoring & Observability

**Required metrics** (export to Prometheus/Datadog):

```rust
use prometheus::{Counter, Gauge, Histogram};

struct SeesawMetrics {
    // Queue depth
    events_pending: Gauge,
    effects_pending: Gauge,

    // Throughput
    events_processed_total: Counter,
    effects_executed_total: Counter,

    // Latency
    event_processing_duration: Histogram,
    effect_execution_duration: Histogram,

    // Errors
    effects_failed_total: Counter,
    dlq_size: Gauge,

    // Worker health
    event_workers_active: Gauge,
    effect_workers_active: Gauge,
}

// Collect every 10s
async fn collect_metrics(pool: &PgPool, metrics: &SeesawMetrics) {
    let pending = sqlx::query!(
        "SELECT COUNT(*) as count FROM seesaw_events WHERE processed_at IS NULL"
    ).fetch_one(pool).await?.count;

    metrics.events_pending.set(pending as f64);

    // ... collect other metrics
}
```

**Dashboards**:
- Queue depth over time (alert if > 10k)
- Effect execution latency p50/p95/p99
- Error rate (alert if > 1%)
- Worker health (alert if workers down)
- DLQ growth rate (alert if growing)

### 2. Alerting

**Critical alerts** (PagerDuty/Opsgenie):

```yaml
alerts:
  - name: QueueDepthHigh
    condition: events_pending > 10000
    severity: warning

  - name: QueueDepthCritical
    condition: events_pending > 100000
    severity: critical

  - name: EffectFailureRateHigh
    condition: effects_failed_total / effects_executed_total > 0.01
    severity: warning

  - name: WorkersDown
    condition: event_workers_active == 0 OR effect_workers_active == 0
    severity: critical

  - name: DLQGrowing
    condition: rate(dlq_size[5m]) > 10
    severity: warning
```

### 3. Operational Runbooks

**Runbook: High Queue Depth**

```
Symptoms: Queue depth > 10k events
Cause: Workers can't keep up with event rate

Steps:
1. Check worker CPU/memory usage
2. Scale effect workers: kubectl scale deployment seesaw-workers --replicas=50
3. Check for stuck effects (long-running queries, API timeouts)
4. Check database connections: SELECT count(*) FROM pg_stat_activity
5. If DB overwhelmed, add connection pooling (PgBouncer)

Prevention:
- Auto-scale based on queue depth
- Set resource limits on effects
- Use priority queue for critical events
```

**Runbook: DLQ Growing**

```
Symptoms: DLQ size increasing
Cause: Effects permanently failing

Steps:
1. Query DLQ for common errors:
   SELECT error, COUNT(*) FROM seesaw_dlq GROUP BY error
2. Check external service health (Stripe, Mailgun)
3. Fix root cause in code
4. Retry from DLQ: engine.retry_from_dlq(event_id, effect_id)

Prevention:
- Better error handling in effects
- Circuit breakers for external services
- Retry with exponential backoff
```

### 4. Testing Strategy

**Load testing** (k6, locust):

```rust
// Simulate 1000 events/sec for 1 hour
for _ in 0..1000 {
    engine.process(OrderPlaced { .. }).await?;
}
```

**Chaos engineering** (kill workers, database):

```rust
#[tokio::test]
async fn test_worker_crashes() {
    // Start workers
    let workers = start_workers().await;

    // Process events
    for i in 0..100 {
        engine.process(Event { id: i }).await?;
    }

    // Kill half the workers
    workers.kill_half().await;

    // Events should still complete (redelivered by reaper)
    wait_for_completion(Duration::from_secs(60)).await?;

    // Verify no data loss
    assert_all_events_processed(&pool).await?;
}
```

### 5. Capacity Planning

**Assumptions** (1M users, 10 events/user/day):

```
Total events/day: 10M
Events/sec: ~115/sec (peak: 500/sec)

Database:
- Events table: ~500MB/day → 15GB/month
- State table: ~100MB (100K active sagas)
- Effect executions: ~1GB/day → 30GB/month

Workers:
- Event workers: 2-5 (CPU-bound, state updates)
- Effect workers: 20-100 (IO-bound, API calls)

Postgres:
- CPU: 8 cores
- Memory: 32GB
- Storage: 500GB SSD
- IOPS: 10k+
```

**Scaling strategy**:
- **0-100k users**: Single Postgres, 2+20 workers
- **100k-1M users**: Read replicas, partitioning, 5+50 workers
- **1M+ users**: Sharding by saga_id, split control/data plane

---

## Decision: GO

**Recommendation**: Build the queue-backed version with production hardening.

**Implementation phases**:
1. PostgresQueue (~200 LOC)
2. PostgresStateStore (~150 LOC)
3. Worker loop (~150 LOC)
4. Idempotency layer (~50 LOC)
5. Production hardening (~200 LOC)
   - State initialization
   - Effect timeouts
   - Visibility timeout reapers (events + effects)
   - Transactional effect intents
   - Graceful shutdown
   - DLQ for failed effects
   - Priority queue support

**Total**: ~700 LOC for production-grade durable execution

**Timeline**: 4 weeks to production-ready implementation

**First milestone**: Multi-day approval workflow example working end-to-end with production hardening

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

1. **Prototype** - Build minimal version in 2 weeks (Phases 1-3)
2. **Harden** - Add production fixes (Phase 3.5)
3. **Validate** - Run multi-day workflow example end-to-end
4. **Measure** - Confirm latency acceptable and no deadlocks
5. **Deploy** - Test graceful shutdown and reaper behavior
6. **Iterate** - Add observability and nice-to-haves as needed

**Start**: Create `crates/seesaw/src/queue.rs` with PostgresQueue

## Design Review Notes

### Gemini Feedback (2026-02-05)

Key contributions from Gemini's architecture review:

1. **Transactional Effect Intents** - Insert effect execution records in same transaction as state update
   - Eliminates crash window between state commit and effect execution
   - Makes effects part of atomic state transition

2. **Zombie Effect Executions** - Reaper needed for both events AND effect_executions table
   - Effects can get stuck in 'executing' status same as events
   - Need periodic cleanup of both tables

3. **idempotency_key Format** - Use UUID v5 for deterministic string keys
   - Required for external APIs like Stripe
   - Deterministic generation from event_id + effect_id

4. **Priority Queue** - Simple addition with high operational value
   - Allows jumping critical events (SystemReset, AdminAction) to front
   - Minimal schema change, big impact

Gemini confirmed:
- ✅ Postgres-only decision is correct
- ✅ SKIP LOCKED pattern is appropriate
- ✅ Compile-time idempotency enforcement is the highlight
- ✅ Design is production-grade with hardening fixes

**Assessment**: "LGO (Lock and Go)" - Architecture is sound, implementation is straightforward
