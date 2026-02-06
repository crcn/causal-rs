# Codebase Audit: What's Already Implemented

**Date:** 2026-02-06
**Finding:** Most of the proposed architecture already exists!

## TL;DR

The Seesaw codebase **already has the pluggable backend architecture** we proposed. The main gaps are:
1. Safety improvements (DistributedSafe trait, docs warnings)
2. Context visibility (execution_mode(), worker_id())
3. Kafka Store implementation (the trait already exists!)

---

## ✅ What's Already Implemented

### 1. **Store Trait** - Fully Implemented

**Location:** `crates/seesaw/src/store.rs`

The `Store` trait is **exactly** what we proposed - a complete abstraction for:

```rust
#[async_trait]
pub trait Store: Send + Sync + 'static {
    // Queue operations
    async fn publish(&self, event: QueuedEvent) -> Result<()>;
    async fn poll_next(&self) -> Result<Option<QueuedEvent>>;
    async fn ack(&self, id: i64) -> Result<()>;
    async fn nack(&self, id: i64, retry_after_secs: u64) -> Result<()>;
    async fn commit_event_processing(&self, commit: EventProcessingCommit) -> Result<()>;

    // Effect execution operations
    async fn insert_effect_intent(...) -> Result<()>;
    async fn poll_next_effect(&self) -> Result<Option<QueuedHandlerExecution>>;
    async fn complete_effect(...) -> Result<()>;
    async fn complete_effect_with_events(...) -> Result<()>;
    async fn fail_effect(...) -> Result<()>;
    async fn dlq_effect(...) -> Result<()>;

    // Join operations (accumulate pattern)
    async fn join_same_batch_append_and_maybe_claim(...) -> Result<Option<Vec<JoinEntry>>>;
    async fn join_same_batch_complete(...) -> Result<()>;
    async fn join_same_batch_release(...) -> Result<()>;
    async fn expire_same_batch_windows(...) -> Result<Vec<ExpiredJoinWindow>>;

    // LISTEN/NOTIFY for workflow events
    async fn subscribe_workflow_events(...) -> Result<Box<dyn Stream<Item = WorkflowEvent>>>;

    // Workflow status
    async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<WorkflowStatus>;
}
```

**Implementations:**
- ✅ `PostgresStore` - Production Postgres backend (`crates/seesaw-postgres/`)
- ✅ `MemoryStore` - Testing/development backend (`crates/seesaw-memory/`)
- ⏸️ `KafkaStore` - **NOT YET IMPLEMENTED** (but trait exists!)

### 2. **QueueBackend Trait** - Fully Implemented

**Location:** `crates/seesaw/src/queue_backend.rs`

Designed specifically for pluggable queue notifications (Kafka/Redis/etc.):

```rust
#[async_trait]
pub trait QueueBackend<St: Store>: Send + Sync + 'static {
    /// Human-readable backend name for diagnostics.
    fn name(&self) -> &'static str;

    /// Called after an effect intent is persisted in the store.
    /// Custom backends can use this hook to enqueue the intent ID.
    async fn on_effect_intent_inserted(
        &self,
        store: &St,
        event_id: Uuid,
        effect_id: &str,
    ) -> Result<()>;

    /// Fetch the next queued effect execution to process.
    async fn poll_next_effect(&self, store: &St) -> Result<Option<QueuedHandlerExecution>>;
}

/// Default queue backend that uses the store for polling
#[derive(Debug, Default, Clone, Copy)]
pub struct StoreQueueBackend;
```

**Key Design:**
- Store remains the **source of truth** for events and state
- QueueBackend is **advisory** - just for notifications/coordination
- Workers fall back to Store polling if backend fails
- Perfect separation: Store = durable state, QueueBackend = hot path

**Usage in Engine:**
```rust
let engine = Engine::new(deps, PostgresStore::new(pool))
    .with_queue_backend(KafkaQueueBackend::new(kafka_config))  // Optional!
    .with_handlers(handlers![...]);
```

### 3. **Engine Architecture** - Implemented

**Location:** `crates/seesaw/src/engine_v2.rs`

Already generic over Store:

```rust
pub struct Engine<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    queue_backend: Arc<dyn QueueBackend<St>>,
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
}

impl<D, St> Engine<D, St> {
    /// Create new engine with dependencies and store
    pub fn new(deps: D, store: St) -> Self { ... }

    /// Override queue backend (optional)
    pub fn with_queue_backend<B>(mut self, queue_backend: B) -> Self
    where B: QueueBackend<St> { ... }

    /// Register handlers
    pub fn with_handler(mut self, handler: Handler<D>) -> Self { ... }
    pub fn with_handlers<I>(mut self, handlers: I) -> Self { ... }

    /// Process event (dispatch to store)
    pub fn process<E>(&self, event: E) -> ProcessFuture<St> { ... }
}
```

**This is exactly the API we proposed!** Users can swap Store implementations transparently.

### 4. **Macro Support** - Comprehensive

**Location:** `crates/seesaw_core_macros/src/lib.rs`

Already supports everything we proposed:

```rust
#[proc_macro_attribute]
pub fn handler(attr: TokenStream, item: TokenStream) -> TokenStream { ... }

#[proc_macro_attribute]
pub fn handlers(attr: TokenStream, item: TokenStream) -> TokenStream { ... }
```

**Supported attributes:**
- ✅ `on = EventType` or `on = [Enum::Variant, ...]`
- ✅ `extract(field1, field2, ...)`
- ✅ `accumulate` - batch accumulation
- ✅ `queued` - background execution
- ✅ `retry = N` - max retry attempts
- ✅ `timeout_secs = N` / `timeout_ms = N`
- ✅ `delay_secs = N` / `delay_ms = N`
- ✅ `priority = N`
- ✅ `id = "handler_id"` or `group = "group_name"`
- ✅ `window_timeout_secs = N` - accumulation window timeout
- ✅ `dlq_terminal = handler_fn` - dead letter mapping

**Example usage (already works):**
```rust
#[handler(on = OrderPlaced, queued, retry = 3, timeout_secs = 30, priority = 1)]
async fn process_order(event: OrderPlaced, ctx: HandlerContext<Deps>) -> Result<OrderShipped> {
    ctx.deps().db.insert_order(&event).await?;
    Ok(OrderShipped { order_id: event.order_id })
}

#[handler(on = RowParsed, accumulate)]
async fn bulk_insert(batch: Vec<RowParsed>, ctx: Ctx) -> Result<BatchInserted> {
    ctx.deps().db.bulk_insert(&batch).await?;
    Ok(BatchInserted { count: batch.len() })
}
```

### 5. **HandlerContext** - Partially Implemented

**Location:** `crates/seesaw/src/handler/context.rs`

Already provides:

```rust
pub struct HandlerContext<D> {
    pub handler_id: String,
    pub idempotency_key: String,
    pub correlation_id: Uuid,
    pub event_id: Uuid,
    pub(crate) deps: Arc<D>,
}

impl<D> HandlerContext<D> {
    pub fn handler_id(&self) -> &str { ... }
    pub fn idempotency_key(&self) -> &str { ... }
    pub fn deps(&self) -> &D { ... }
    pub fn current_event_id(&self) -> Uuid { ... }
}
```

**Missing (from our proposal):**
- ❌ `execution_mode() -> ExecutionMode`
- ❌ `is_background() -> bool`
- ❌ `worker_id() -> Option<&str>`

---

## ❌ What's Missing

### 1. **DistributedSafe Trait** - Not Implemented

No compile-time safety for `Arc<Mutex>` in distributed deps.

**Proposed:**
```rust
mod sealed {
    pub trait Sealed {}
}

pub trait DistributedSafe: Clone + Send + Sync + sealed::Sealed + 'static {}

// Auto-implement for known-safe types
impl sealed::Sealed for PgPool {}
impl DistributedSafe for PgPool {}

// Derive macro to validate fields
#[derive(Clone, DistributedSafe)]
struct Deps {
    db: PgPool,  // ✅
    #[allow_non_distributed]
    cache: Arc<Mutex<HashMap>>,  // ⚠️ Warning
}
```

### 2. **Explicit `queued` Requirement** - Not Enforced

Macro currently allows:
```rust
#[handler(on = Event, retry = 3)]  // Silently becomes background
```

**Should require:**
```rust
#[handler(on = Event, queued, retry = 3)]  // Explicit
```

### 3. **Engine Configuration Methods** - Not Implemented

No `.single_worker()` or `.distributed()` methods:

**Proposed:**
```rust
impl<D> Engine<D, St> {
    pub fn single_worker(self) -> Self { ... }
}

impl<D: DistributedSafe> Engine<D, St> {
    pub fn distributed(self, config: WorkerConfig) -> Self { ... }
}
```

### 4. **Documentation Gaps** - Critical

**Missing:**
- ⚠️ No warning about `Arc<Mutex>` breaking with multiple workers
- ⚠️ No transaction boundary explanation
- ⚠️ No single-process vs multi-worker capability table
- ⚠️ No clear "What Seesaw Is/Isn't" positioning

### 5. **KafkaStore Implementation** - Not Implemented

The **trait exists**, just need the implementation:

**Required:**
```rust
pub struct KafkaStore {
    // Event queue
    producer: FutureProducer,
    consumer: StreamConsumer,
    topic: String,

    // State storage (still need Postgres/Redis!)
    state_pool: PgPool,
}

#[async_trait]
impl Store for KafkaStore {
    async fn publish(&self, event: QueuedEvent) -> Result<()> {
        // Publish to Kafka topic
        self.producer.send(...).await
    }

    async fn join_same_batch_append_and_maybe_claim(...) -> Result<...> {
        // Join state still goes to Postgres
        sqlx::query("INSERT INTO seesaw_join_entries ...")
            .execute(&self.state_pool)
            .await
    }

    // ... all state operations use state_pool
}
```

**Optional:** `KafkaQueueBackend` for notifications (Store handles persistence).

---

## 📊 Implementation Status Summary

| Feature | Status | Priority | Effort |
|---------|--------|----------|--------|
| **Architecture** |
| Store trait | ✅ Done | - | - |
| QueueBackend trait | ✅ Done | - | - |
| Engine<D, St> | ✅ Done | - | - |
| PostgresStore | ✅ Done | - | - |
| MemoryStore | ✅ Done | - | - |
| KafkaStore | ❌ Missing | Medium | Medium |
| **Macros** |
| #[handler] | ✅ Done | - | - |
| All attributes | ✅ Done | - | - |
| Explicit `queued` check | ❌ Missing | High | Low |
| **Safety** |
| DistributedSafe trait | ❌ Missing | High | Medium |
| Derive macro | ❌ Missing | High | Medium |
| Engine `.distributed()` | ❌ Missing | Medium | Low |
| **Context** |
| Basic context | ✅ Done | - | - |
| execution_mode() | ❌ Missing | Low | Low |
| worker_id() | ❌ Missing | Low | Low |
| **Documentation** |
| Arc<Mutex> warning | ❌ Missing | **CRITICAL** | Low |
| Transaction boundaries | ❌ Missing | High | Low |
| Capability table | ❌ Missing | High | Low |
| Examples organization | ❌ Missing | Medium | Medium |

---

## 🎯 Revised Implementation Plan

### Phase 1: Documentation (Week 1) - **UNCHANGED**

Already critical, now even more important:

1. **Add Arc<Mutex> warning** - Biggest footgun, needs immediate docs fix
2. **Document transaction boundaries** - Users need to understand execution model
3. **Add capability table** - Single-process vs multi-worker comparison
4. **Update state management section** - Remove Arc<Mutex> from recommended patterns

### Phase 2: Safety (Week 2) - **NEW PRIORITY**

Now easier because architecture exists:

1. **Add DistributedSafe trait** to `crates/seesaw/src/lib.rs`
2. **Create derive macro** in `crates/seesaw_core_macros/`
3. **Add `.single_worker()` and `.distributed()`** to Engine
4. **Enforce `queued` requirement** in macro (add validation)

### Phase 3: Context Visibility (Week 3) - **LOW PRIORITY**

Nice-to-have, not critical:

1. Add `execution_mode()`, `is_background()`, `worker_id()` to HandlerContext
2. Thread execution context through workers
3. Update examples to demonstrate usage

### Phase 4: Kafka Implementation (Future) - **OPTIONAL**

Now straightforward - just implement the trait:

1. Create `crates/seesaw-kafka/` crate
2. Implement `KafkaStore` (Store trait)
3. Optionally implement `KafkaQueueBackend` (QueueBackend trait)
4. Add integration tests
5. Document trade-offs

---

## 🎉 Key Insights

### The Good News

1. **Architecture is already pluggable!** The Store trait is exactly what we need.
2. **Engine is already generic!** Engine<D, St> allows any Store implementation.
3. **Macros are comprehensive!** All the attributes we proposed already exist.
4. **Multiple Store implementations exist!** PostgresStore and MemoryStore prove the pattern works.

### The Gaps

1. **Documentation** is the biggest gap - users don't know about distributed constraints
2. **Safety** is missing - no compile-time protection against Arc<Mutex> footguns
3. **Kafka** is just an implementation detail - the trait already exists

### The Plan

**Focus on safety and docs first**, then optionally add Kafka Store implementation.

The architecture is solid. We just need to:
1. Document the constraints clearly
2. Add compile-time safety checks
3. Optionally implement KafkaStore when needed

---

## 📝 Updated Recommendations

### For Users Today

**Good for:**
- ✅ Single-process or multi-worker with PostgresStore
- ✅ Event-driven reactive systems within bounded context
- ✅ Background job processing with retry

**Watch out for:**
- ⚠️ Arc<Mutex> in deps breaks with multiple workers (needs docs!)
- ⚠️ Transaction boundaries differ (inline vs background)
- ⚠️ Scalability ceiling at ~50k events/sec, ~500 workers

### For Future Work

1. **Document Arc<Mutex> danger** - Critical immediate fix
2. **Add DistributedSafe trait** - Prevent footguns at compile time
3. **Implement KafkaStore** - When users hit Postgres scalability limits

The foundation is excellent. We just need safety guardrails and clearer documentation.
