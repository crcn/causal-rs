# Distributed Architecture Assessment and API Improvements

**Date:** 2026-02-06
**Status:** Proposed
**Discussion:** Based on distributed systems analysis and OpenAI feedback

## Executive Summary

Seesaw is a **Postgres-backed distributed worker pool** with event-driven handlers, not a distributed system in the traditional sense (like Kafka, Akka, or microservices). It's comparable to Sidekiq, Celery, or BullMQ but with an event-driven programming model.

**Current capabilities:**
- ✅ Multi-worker deployment (horizontal scaling to ~500 workers)
- ✅ Event-driven reactive programming
- ✅ Durable queue with retry
- ✅ Strong consistency within bounded context

**Limitations:**
- ❌ Single Postgres SPOF
- ❌ No service boundaries (monolithic)
- ❌ Bounded scalability (~50k events/sec)
- ❌ No network partition tolerance
- ❌ Single datacenter deployment

**This document covers:**
1. Distributed systems assessment
2. Proposed API improvements (based on feedback)
3. Kafka backend compatibility analysis
4. Implementation roadmap

---

## Part 1: Distributed Systems Assessment

### What Seesaw Actually Is

Seesaw is a **centralized task queue with event-driven handlers**:

```
┌─────────────────────────────────────┐
│     Seesaw Engine (Single Service)  │
│                                     │
│  Worker 1 ┐                         │
│  Worker 2 ├─→ Postgres Queue        │
│  Worker 3 ┘    (pg_notify)          │
│                                     │
│  ┌──────────┐  ┌──────────┐       │
│  │ Handler  │  │ Handler  │       │
│  │  Pool    │  │  Pool    │       │
│  └──────────┘  └──────────┘       │
└─────────────────────────────────────┘
```

**Similar to:**
- Sidekiq/Resque (Ruby)
- Celery (Python)
- BullMQ (Node.js)
- Hangfire (.NET)

**Not similar to:**
- Kafka (event streaming)
- Temporal/Cadence (workflow engines)
- Akka/Orleans (actor systems)
- Microservices architecture

### Comparison to Real Distributed Systems

#### vs **Kafka / Event Streaming**

| Feature | Seesaw | Kafka |
|---------|--------|-------|
| Partitioning | ❌ None | ✅ Topic partitions |
| Consumer groups | ❌ None | ✅ Yes |
| Replay | ❌ No | ✅ Offset-based |
| Retention | ❌ Process once | ✅ Configurable |
| Throughput | ~10k-50k/sec | Millions/sec |
| Latency | ~50-100ms | ~5-10ms |
| Ops complexity | ✅ Low (Postgres) | ❌ High (brokers, ZK) |

#### vs **Temporal / Cadence**

| Feature | Seesaw | Temporal |
|---------|--------|----------|
| Workflow DSL | ❌ No | ✅ Yes |
| Long-running sagas | ⚠️ Manual | ✅ Built-in |
| Timeouts/retries | ⚠️ Per-handler | ✅ Workflow-level |
| Versioning | ❌ No | ✅ Yes |
| Multi-service | ❌ No | ✅ Yes |
| Simplicity | ✅ High | ❌ Low |

#### vs **Akka / Orleans (Actors)**

| Feature | Seesaw | Akka |
|---------|--------|------|
| Actor addressing | ❌ No | ✅ Yes |
| Location transparency | ❌ No | ✅ Yes |
| Cluster membership | ❌ No | ✅ Gossip protocol |
| State | Postgres | In-memory + snapshots |
| Distributed | Single Postgres | Multi-node cluster |

### Fundamental Limitations

#### 1. **Single Point of Failure**
```
All Workers → Single Postgres Instance
                     ↓
                  (SPOF)
```

If Postgres goes down, entire system stops. No redundancy strategy.

#### 2. **Bounded Scalability**

```
Scaling ceiling:
- Postgres connection limit (~100-500 workers max)
- Single DB write throughput (~10k-50k events/sec)
- pg_notify contention at scale
```

#### 3. **No Network Partition Handling**

The design assumes:
- ✅ Workers can always reach Postgres
- ✅ Network is reliable
- ❌ No CAP theorem considerations
- ❌ No split-brain handling

#### 4. **No Service Boundaries**

All handlers in same system, same Postgres instance:

```rust
// All tightly coupled in one Engine
let engine = Engine::new(deps, store)
    .with_handlers(handlers![
        order_handlers::handlers(),
        payment_handlers::handlers(),
        shipping_handlers::handlers(),
    ]);
```

#### 5. **No Partitioning Strategy**

All events go to shared queue, any worker can pick up any event.

### When to Use Seesaw

#### ✅ **Good For:**

**Monolithic event-driven applications:**
```rust
// Single service with complex internal workflows
OrderService {
    - Event-driven order processing
    - Multiple workers for throughput
    - Strong consistency needs
    - Simple ops (just Postgres)
}
```

**Background job processing within bounded context:**
```rust
EmailService {
    - Queue emails for sending
    - Retry failed sends
    - Track delivery status
    - All in one service boundary
}
```

**Replacing ad-hoc job queues:**
- Instead of: Redis + custom worker code
- Use: Seesaw with event model

#### ❌ **Not Good For:**

**Microservices architecture:**
```rust
// DON'T: Share Postgres across services
OrderService (Seesaw) → shared Postgres ← PaymentService (Seesaw)

// DO: Separate services with message broker
OrderService (Seesaw) → Kafka ← PaymentService (Seesaw)
```

**High-throughput streaming** (>100k events/sec):
- Postgres can't keep up
- pg_notify becomes bottleneck

**Multi-datacenter / geo-distributed:**
- Single Postgres instance
- Network latency kills performance

**Network partition tolerance:**
- No CAP theorem handling
- Assumes reliable network

---

## Part 2: Proposed API Improvements

Based on OpenAI feedback, prioritized for safety and clarity.

### Phase 1: Documentation Safety Pass (Week 1)

#### 1.1 Fix `Arc<Mutex>` Documentation ⚠️ CRITICAL

**Problem:** Docs show `Arc<Mutex<HashMap>>` as valid pattern, but it silently breaks with multiple workers.

**Action:** Update CLAUDE.md to mark as single-process only:

```markdown
### ❌ Pattern 4: In-Memory Shared State (SINGLE-PROCESS ONLY)

**⚠️ WARNING**: This pattern does NOT work with multiple workers!

```rust
// ❌ BREAKS with multiple workers - DO NOT USE in production
#[derive(Clone)]
struct Deps {
    cache: Arc<Mutex<HashMap<Uuid, Status>>>,  // Each worker has separate copy!
}

// Worker 1: cache[id] = Status::A
// Worker 2: cache[id] = Status::B
// Workers diverge silently! ☠️
```

**Only use this pattern when:**
- ✅ You have exactly ONE worker process
- ✅ You will NEVER scale horizontally
- ✅ You need ultra-low-latency in-memory state

**For production systems**: Use Pattern 2 (external persistence) instead.
```

**Recommended patterns:**
1. **State in events** - State flows through event fields (distributed-safe)
2. **External storage** - Postgres, Redis (distributed-safe)
3. **Implicit state** - Event sequences (distributed-safe)

#### 1.2 Document Transaction Boundaries

Add clear section explaining when handlers run in which transaction:

**Inline Handlers (Default):**
```rust
engine.dispatch(OrderPlaced { id: 123 }).await?;
// ↓ [Transaction begins]
// ↓ Handler executes
// ↓ Emits OrderShipped (inserted in same TX)
// ↓ [Transaction commits - atomic]
```

**Background Handlers:**
```rust
#[handler(on = PaymentRequested, retry = 3, queued)]
async fn charge_payment(...) { }

// 1. Event inserted into queue (transaction A)
// 2. Worker picks up event
// 3. [Transaction B begins]
// 4. Handler executes
// 5. Emits new events (same TX B)
// 6. [Transaction B commits]
```

**Key point:** DON'T expose raw transaction handles in public API (couples to SQL backend).

#### 1.3 Add Capability Table

```markdown
| Feature | Single-Process | Multi-Worker | Notes |
|---------|----------------|--------------|-------|
| **State Management** |
| State in events | ✅ | ✅ | Recommended |
| External storage | ✅ | ✅ | DB, Redis, etc. |
| `Arc<Mutex>` in deps | ✅ | ❌ | Dev only |
| **Execution** |
| Inline handlers | ✅ | ✅ | Same transaction |
| Background handlers | ✅ | ✅ | Queued, retryable |
| Horizontal scaling | ❌ | ✅ | Add workers |
| **Reliability** |
| Survives crashes | ⚠️ | ✅ | Durable queue |
| **When to Use** |
| Development | ✅ | | Simpler |
| Production | | ✅ | Reliable + scale |
```

### Phase 2: Explicit Execution Mode (Week 2)

#### 2.1 Add `queued` Attribute Requirement

**Problem:** Mode switches implicitly based on config (`.retry(3)` silently becomes background).

**Current:**
```rust
#[handler(on = Event, retry = 3)]  // Silently becomes background
async fn handler(...) { }
```

**Proposed:**
```rust
// Explicit inline (default)
#[handler(on = Event)]
async fn fast_handler(...) { }

// Explicit background - MUST add queued
#[handler(on = Event, queued, retry = 3)]
async fn slow_handler(...) { }

// Compile error if mixing
#[handler(on = Event, inline, retry = 3)]  // ❌ ERROR
async fn bad_handler(...) { }
```

#### 2.2 Update Macro to Enforce

In `seesaw_core_macros/src/lib.rs`:

```rust
fn parse_effect_args(metas: &Punctuated<Meta, Token![,]>) -> syn::Result<EffectArgs> {
    // ... existing code ...

    // After parsing all attributes:
    if args.inline && effect_requires_background(&args) {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "inline handlers cannot use retry, timeout, delay, or priority (use queued instead)",
        ));
    }

    if effect_requires_background(&args) && !args.queued {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "handlers with retry/timeout/delay/priority must be explicitly marked 'queued'",
        ));
    }

    Ok(args)
}

fn effect_requires_background(args: &EffectArgs) -> bool {
    args.retry.unwrap_or(1) > 1
        || args.timeout_secs.is_some()
        || args.timeout_ms.is_some()
        || args.delay_secs.is_some()
        || args.delay_ms.is_some()
        || args.priority.is_some()
}
```

### Phase 3: Distributed Safety Enforcement (Week 3)

#### 3.1 Sealed `DistributedSafe` Trait

**Design:** Sealed trait + derive macro + unsafe opt-out

```rust
// In seesaw_core/src/distributed_safe.rs

mod sealed {
    pub trait Sealed {}
}

/// Marker trait for types safe to share across distributed workers.
///
/// ## Safety
/// Types implementing this trait MUST NOT contain:
/// - `Arc<Mutex<T>>` or other in-memory shared state
/// - File handles or OS resources
/// - Thread-local storage
///
/// Safe types include:
/// - Database connection pools (PgPool, etc.)
/// - HTTP clients
/// - Redis clients
/// - Stateless services
pub trait DistributedSafe: Clone + Send + Sync + sealed::Sealed + 'static {}

// Auto-implement for known-safe types
impl sealed::Sealed for sqlx::PgPool {}
impl DistributedSafe for sqlx::PgPool {}

impl sealed::Sealed for reqwest::Client {}
impl DistributedSafe for reqwest::Client {}
```

#### 3.2 Derive Macro

```rust
// In seesaw_core_macros

#[proc_macro_derive(DistributedSafe, attributes(allow_non_distributed))]
pub fn derive_distributed_safe(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    // For each field:
    // - Check if has #[allow_non_distributed] attribute
    // - If not, verify field type implements DistributedSafe
    // - Emit helpful error if not

    // Generate impl
    quote! {
        impl ::seesaw_core::sealed::Sealed for #name {}
        impl ::seesaw_core::DistributedSafe for #name {}
    }.into()
}
```

#### 3.3 Engine Enforces

```rust
// Runtime config
pub struct WorkerConfig {
    pub worker_id: String,
    pub total_workers: usize,
    pub heartbeat_interval: Duration,
}

impl<D> Engine<D> {
    // Single-worker mode - no restrictions
    pub fn single_worker(self) -> Self {
        self.config.distributed = false;
        self
    }
}

// Distributed mode - requires DistributedSafe
impl<D: DistributedSafe> Engine<D> {
    pub fn distributed(self, config: WorkerConfig) -> Self {
        self.config.worker_config = Some(config);
        self
    }
}
```

### Phase 4: Context Visibility (Week 4)

#### 4.1 Add Worker Context Methods

```rust
pub trait HandlerContext<D> {
    // Existing
    fn deps(&self) -> &D;
    fn handler_id(&self) -> Uuid;
    fn idempotency_key(&self) -> String;
    fn correlation_id(&self) -> Uuid;
    fn event_id(&self) -> Uuid;

    // NEW
    fn execution_mode(&self) -> ExecutionMode;
    fn is_background(&self) -> bool;
    fn worker_id(&self) -> Option<&str>;
}

pub enum ExecutionMode {
    Inline,      // Same transaction as dispatcher
    Background,  // Separate worker transaction
}
```

#### 4.2 Usage Examples

```rust
#[handler(on = Event)]
async fn adaptive_handler(event: Event, ctx: Ctx) -> Result<NextEvent> {
    let timeout = match ctx.execution_mode() {
        ExecutionMode::Inline => Duration::from_secs(1),
        ExecutionMode::Background => Duration::from_secs(300),
    };

    timeout(timeout, work()).await??;
    Ok(NextEvent)
}
```

### Phase 5: Examples Organization

Reorganize examples directory:

```
examples/
├── README.md
├── single-process/
│   ├── simple-pipeline/
│   ├── in-memory-state/          # Arc<Mutex> - dev only
│   └── inline-handlers/
├── distributed/
│   ├── event-threaded-state/
│   ├── external-state/
│   ├── batch-processing/
│   ├── multi-worker/
│   └── cross-domain/
└── patterns/
    ├── error-handling/
    ├── compensation/
    ├── request-response/
    └── retry-strategies/
```

---

## Part 3: Kafka Backend Compatibility

### Overview

**Key Insight:** Kafka support should be a **backend implementation detail**, not a user-facing API change.

**Design Principle:** Handlers remain unchanged. Only Engine construction differs.

### User-Facing API (Stays Identical)

```rust
// Handlers don't change AT ALL
#[handler(on = OrderPlaced, queued, retry = 3)]
async fn process_order(event: OrderPlaced, ctx: HandlerContext<Deps>) -> Result<OrderShipped> {
    ctx.deps().db.insert_order(&event).await?;
    Ok(OrderShipped { order_id: event.order_id })
}

// Only difference: Engine construction
// Postgres backend (default)
let engine = Engine::new(deps, PostgresStore::new(pool));

// Kafka backend (same handler API!)
let engine = Engine::new(deps, KafkaStore::new(kafka_config, state_pool));

// Redis Streams backend (future)
let engine = Engine::new(deps, RedisStore::new(redis_config, state_pool));
```

**Zero handler code changes** when switching backends.

### Architecture Comparison

**Current (Postgres):**
```
Events → PostgresStore ← Workers
         │
         ├─ Queue (seesaw_queue)
         ├─ State (join windows)
         ├─ Idempotency
         └─ pg_notify
```

**With Kafka:**
```
Events → KafkaStore ← Workers
         ├─ Queue: Kafka (partitioned topics)
         └─ State: Postgres (join, idempotency)

All state operations → Postgres
All event queue → Kafka
```

**Key:** State always needs a database, regardless of event queue backend.

### Store Trait Abstraction

```rust
#[async_trait]
pub trait Store: Clone + Send + Sync + 'static {
    // Event queue operations
    async fn enqueue_event(&self, event: &dyn Event) -> Result<EventId>;
    async fn dequeue_events(&self, worker_id: &str) -> Result<Vec<QueuedEvent>>;
    async fn mark_event_processed(&self, event_id: EventId) -> Result<()>;

    // State operations (always needed regardless of queue)
    async fn save_join_entry(&self, entry: JoinEntry) -> Result<()>;
    async fn get_join_window(&self, window_id: Uuid) -> Result<Option<JoinWindow>>;
    async fn mark_window_complete(&self, window_id: Uuid) -> Result<()>;
    async fn save_idempotency_key(&self, key: &str, ttl: Duration) -> Result<()>;
    async fn check_idempotency(&self, key: &str) -> Result<bool>;
    async fn save_handler_execution(&self, exec: HandlerExecution) -> Result<()>;
}
```

### PostgresStore Implementation (Current)

```rust
pub struct PostgresStore {
    pool: PgPool,
}

#[async_trait]
impl Store for PostgresStore {
    async fn enqueue_event(&self, event: &dyn Event) -> Result<EventId> {
        // All in Postgres
        let event_id = sqlx::query_scalar(
            "INSERT INTO seesaw_queue (event_type, payload) VALUES ($1, $2) RETURNING id"
        )
        .bind(event.type_name())
        .bind(serde_json::to_value(event)?)
        .fetch_one(&self.pool)
        .await?;

        // Notify workers
        sqlx::query("NOTIFY seesaw_events").execute(&self.pool).await?;

        Ok(event_id)
    }

    async fn save_join_entry(&self, entry: JoinEntry) -> Result<()> {
        // Join state in same database
        sqlx::query("INSERT INTO seesaw_join_entries ...")
            .execute(&self.pool).await?;
        Ok(())
    }

    // ... all operations use same pool
}
```

### KafkaStore Implementation (Hybrid)

```rust
pub struct KafkaStore {
    // Event queue
    producer: FutureProducer,
    consumer: StreamConsumer,
    topic: String,

    // State storage (still Postgres!)
    state_pool: PgPool,
}

#[async_trait]
impl Store for KafkaStore {
    async fn enqueue_event(&self, event: &dyn Event) -> Result<EventId> {
        // Events go to Kafka
        let event_id = Uuid::new_v4();
        let payload = serde_json::to_vec(event)?;

        self.producer.send(
            FutureRecord::to(&self.topic)
                .key(&event.partition_key())
                .payload(&payload)
                .headers(OwnedHeaders::new()
                    .insert(Header {
                        key: "event_id",
                        value: Some(&event_id.to_string()),
                    })),
            Duration::from_secs(5),
        ).await?;

        Ok(event_id)
    }

    async fn dequeue_events(&self, worker_id: &str) -> Result<Vec<QueuedEvent>> {
        // Consumer group handles work distribution
        // Kafka automatically assigns partitions to workers
        let messages = self.consumer.stream().next().await;
        // ... deserialize events
    }

    async fn save_join_entry(&self, entry: JoinEntry) -> Result<()> {
        // Join state still goes to Postgres
        sqlx::query("INSERT INTO seesaw_join_entries ...")
            .execute(&self.state_pool)  // Note: state_pool, not Kafka
            .await?;
        Ok(())
    }

    async fn check_idempotency(&self, key: &str) -> Result<bool> {
        // Idempotency still in Postgres
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM idempotency_keys WHERE key = $1)")
            .bind(key)
            .fetch_one(&self.state_pool)
            .await
    }

    // ... all state operations use state_pool
}
```

### What Changes vs What Stays Same

#### ✅ **Handlers Stay Identical**

```rust
// This code NEVER changes regardless of backend
#[handler(on = RowParsed, accumulate)]
async fn bulk_insert(batch: Vec<RowParsed>, ctx: Ctx) -> Result<BatchInserted> {
    ctx.deps().db.bulk_insert(&batch).await?;
    Ok(BatchInserted { count: batch.len() })
}

// Works with PostgresStore ✅
// Works with KafkaStore ✅
// Works with RedisStore ✅
```

#### ✅ **User Dependencies Stay Same**

```rust
#[derive(Clone)]
struct Deps {
    db: PgPool,      // Application data
    http: Client,    // External APIs
}

// Deps don't know or care about Store backend
```

#### 🔄 **Only Engine Construction Changes**

```rust
// Development: Postgres backend
let engine = Engine::new(
    deps,
    PostgresStore::new(pool),
);

// Production: Kafka backend for high throughput
let engine = Engine::new(
    deps,
    KafkaStore::new(
        kafka_config,
        state_pool,  // Still need Postgres for state
    ),
);
```

### What Kafka Solves

#### 1. **Throughput** ✅

```
PostgresStore:  ~10k-50k events/sec
KafkaStore:     ~1M+ events/sec
```

#### 2. **Horizontal Scaling** ✅

**PostgresStore:**
- ~500 workers max (connection pool)
- Workers poll same queue

**KafkaStore:**
- 1000s of workers
- Consumer groups with automatic partition assignment
- Workers process different partitions

#### 3. **Replay** ✅

**PostgresStore:**
- Events deleted after processing

**KafkaStore:**
- Events retained (configurable)
- Can replay from any offset
- Time-travel: "replay from 3 days ago"

#### 4. **Multi-Datacenter** ✅

**PostgresStore:**
- Single datacenter

**KafkaStore:**
- Cross-DC replication
- Active-active deployments

### What Kafka Doesn't Solve

#### 1. **Still Need Database** ❌

```rust
// Accumulate pattern requires queryable state
#[handler(on = RowParsed, accumulate)]
async fn bulk_insert(batch: Vec<RowParsed>, ctx: Ctx) -> Result<BatchInserted> {
    // Kafka is a log, not a database
    // Can't efficiently query: "give me all events for batch_id X"

    // Join state, idempotency, handler tracking all need Postgres/Redis
    ctx.deps().db.bulk_insert(&batch).await?;
    Ok(BatchInserted { count: batch.len() })
}
```

Even with KafkaStore, you need Postgres (or Redis) for:
- `seesaw_join_entries` (accumulate state)
- `seesaw_join_windows` (window tracking)
- `idempotency_keys` (deduplication)
- `handler_executions` (retry counts)

#### 2. **Operational Complexity** ❌

**PostgresStore:**
- 1 system (Postgres)
- Simple monitoring
- Local dev: `docker-compose up postgres`

**KafkaStore:**
- 2 systems (Kafka + Postgres)
- Kafka cluster (3+ brokers)
- ZooKeeper/KRaft
- Consumer group coordination
- Offset management
- Local dev: `docker-compose up kafka zookeeper postgres`

#### 3. **Transaction Semantics** ⚠️

**PostgresStore (Atomic):**
```rust
engine.dispatch(OrderPlaced { id: 123 }).await?;
  ↓ [Single Postgres Transaction]
  ↓ Insert event into queue
  ↓ Handler runs (inline)
  ↓ Insert OrderShipped into queue
  ↓ [Commit - all or nothing]
```

**KafkaStore (Eventually Consistent):**
```rust
engine.dispatch(OrderPlaced { id: 123 }).await?;
  ↓ [Kafka write - committed]
  ↓ Worker picks up (async)
  ↓ [Separate transaction]
  ↓ Handler runs
  ↓ [Another Kafka write]

// Can't guarantee atomicity across Kafka + Postgres
// Need idempotency + retry for reliability
```

### When to Use Each Store

#### Use **PostgresStore** When:
- ✅ Throughput < 50k events/sec
- ✅ Workers < 500
- ✅ Single datacenter
- ✅ Need strong consistency (atomic dispatch+handler)
- ✅ Want operational simplicity
- ✅ Small team / startup

#### Use **KafkaStore** When:
- ✅ Throughput > 100k events/sec
- ✅ Workers > 1000
- ✅ Need event replay/time-travel
- ✅ Multi-datacenter deployment
- ✅ Already have Kafka infrastructure
- ✅ Large team with ops expertise
- ⚠️ Can tolerate eventual consistency
- ⚠️ Can handle operational complexity

### Progressive Migration Path

```rust
// Stage 1: Start with Postgres (simple)
let engine = Engine::new(deps, PostgresStore::new(pool));

// Stage 2: Growth - still Postgres, optimize queries
// Add indexes, tune connection pool, vertical scaling

// Stage 3: Hit scaling ceiling - evaluate need
// Do you actually need >50k events/sec?
// Can you partition into multiple services instead?

// Stage 4: If truly needed - migrate to Kafka
let engine = Engine::new(
    deps,
    KafkaStore::new(kafka_config, state_pool),
);

// ZERO HANDLER CODE CHANGES 🎉
```

### Cross-Service Architecture

For microservices, use multiple Seesaw instances with Kafka **between** them:

```rust
// Order Service - internal events use PostgresStore
let order_engine = Engine::new(
    order_deps,
    PostgresStore::new(order_db),
);

#[handler(on = OrderPlaced)]
async fn process_order_internal(event: OrderPlaced, ctx: Ctx) -> Result<OrderProcessed> {
    // Internal processing
    ctx.deps().db.insert_order(&event).await?;

    // Publish to Kafka for other services (via deps, not Store)
    ctx.deps().kafka.publish("order-events", OrderPlacedExternal {
        order_id: event.order_id,
    }).await?;

    Ok(OrderProcessed { order_id: event.order_id })
}

// Payment Service - separate database, listens to Kafka
let payment_engine = Engine::new(
    payment_deps,
    PostgresStore::new(payment_db),  // Own Postgres
);

// Listen to external Kafka topic via deps
ctx.deps().kafka_consumer.subscribe("order-events");
// Or use KafkaStore if Payment Service has high internal throughput
```

**Key:** Kafka used for **inter-service** communication, not required for **intra-service** events.

---

## Implementation Roadmap

### Week 1: Documentation (HIGHEST PRIORITY)
- [ ] Remove/mark `Arc<Mutex>` pattern as single-process only
- [ ] Add transaction boundaries section
- [ ] Add capability table (single-process vs multi-worker)
- [ ] Update state management section
- [ ] Add "What Seesaw Is/Isn't" section

### Week 2: Execution Mode
- [ ] Add `inline` attribute to macro
- [ ] Make `queued` required for background handlers
- [ ] Add compile-time enforcement
- [ ] Update all examples

### Week 3: Distributed Safety
- [ ] Implement sealed `DistributedSafe` trait
- [ ] Create derive macro with field validation
- [ ] Add `.single_worker()` and `.distributed()` to Engine
- [ ] Update deps validation

### Week 4: Context & Examples
- [ ] Add `execution_mode()`, `is_background()`, `worker_id()` to HandlerContext
- [ ] Reorganize examples directory
- [ ] Write distributed deployment guide
- [ ] Add performance benchmarks

### Future: Kafka Backend (Optional)
- [ ] Design pluggable backend trait
- [ ] Implement Kafka backend
- [ ] Add outbox pattern support
- [ ] Document trade-offs
- [ ] Add Kafka examples

---

## Breaking Changes Summary

### Required Code Changes

1. **Add `queued` to background handlers:**
```rust
// Before
#[handler(on = Event, retry = 3)]

// After
#[handler(on = Event, queued, retry = 3)]
```

2. **Use `DistributedSafe` for multi-worker:**
```rust
#[derive(Clone, DistributedSafe)]
struct Deps {
    db: PgPool,
}

let engine = Engine::new(deps, store)
    .distributed(WorkerConfig { ... });
```

3. **Cannot use `Arc<Mutex>` in multi-worker:**
```rust
// Compile error unless using .single_worker()
```

### Non-Breaking (Backward Compatible)

1. New context methods (`execution_mode()`, etc.)
2. Engine config methods (`.single_worker()`, `.distributed()`)
3. `inline` attribute (optional)

---

## Conclusion

### Current State

Seesaw is a **well-designed Postgres-backed distributed worker pool** that excels at:
- Event-driven programming within a bounded context
- Background job processing with retry
- Strong consistency guarantees
- Operational simplicity

### Limitations

It is NOT a general-purpose distributed systems framework:
- Single Postgres SPOF
- Bounded scalability (~500 workers, ~50k events/sec)
- No service boundaries
- Single datacenter

### Proposed Improvements

The API changes make it:
- ✅ Safer for multi-worker deployment
- ✅ Clearer about execution models
- ✅ More explicit about distributed constraints

But they don't fundamentally change the architecture's scope.

### Kafka Backend

Kafka compatibility is possible and valuable for:
- High-throughput scenarios (>100k events/sec)
- Multi-datacenter deployments
- Event replay requirements

But adds significant complexity and requires outbox pattern.

### Positioning

Seesaw should be positioned as:
- ✅ "Distributed worker pool for event-driven systems"
- ✅ "Postgres-backed reactive task queue"
- ✅ "Event-driven background job processor"

Not as:
- ❌ "Distributed event streaming platform"
- ❌ "Microservices orchestration framework"
- ❌ "Distributed actor system"

This is honest about its scope and sets appropriate expectations.
