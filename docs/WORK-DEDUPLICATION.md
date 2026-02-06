# Seesaw Work Deduplication Architecture

## Overview

Seesaw implements **at-most-once processing semantics** with multi-layered deduplication guarantees. Work is deduplicated at the event, handler execution, and external API levels through database constraints, deterministic ID generation, and idempotency keys.

> **Version**: v0.10.3 (schema v0.9.1, updated 2026-02-06)
> **Status**: Effect→Handler refactor complete. All core APIs use Handler terminology.
> **Minor inconsistencies**: Some internal parameter names still use `effect_id` (e.g., QueueBackend trait) but don't impact public API.

## Deduplication Layers

### 1. Event-Level Deduplication

**Mechanism**: Two-tier ledger pattern with database constraints

**Files**:
- `docs/schema.sql` (lines 89-101) - `seesaw_processed` ledger table
- `docs/schema.sql` (lines 32-35) - `UNIQUE INDEX idx_events_event_id`
- `crates/seesaw-postgres/src/lib.rs` (lines 111-155) - `publish()` implementation

**How it works**:
```rust
// Step 1: Try to insert into seesaw_processed ledger
INSERT INTO seesaw_processed (event_id, correlation_id, created_at)
VALUES ($1, $2, $3)
ON CONFLICT (event_id) DO NOTHING
RETURNING event_id

// Step 2: If returned nothing → event already exists → return early (idempotent)
// Step 3: Only if insertion succeeds → insert into seesaw_events queue
```

**Guarantees**:
- ✅ Same event_id published multiple times = only one insertion
- ✅ Webhook retries with different timestamps = deduplicated
- ✅ Crash+retry scenarios = idempotent via PRIMARY KEY constraint
- ✅ Concurrent workers publishing same event = one wins, others skip

**Test coverage**: `crates/seesaw-postgres/tests/integration_tests.rs`
- `test_idempotent_publish()` (lines 92-128)
- `test_publish_deduplicates_event_id_even_with_different_timestamps()` (lines 317-368)

---

### 2. Deterministic Event ID Generation

**Mechanism**: UUID v5 hashing for crash-safe event generation

**Files**:
- `crates/seesaw/src/store.rs` (lines 8-11) - `NAMESPACE_SEESAW` constant
- `crates/seesaw/src/runtime/event_worker.rs` (lines 391-398) - Inline handler emission
- `crates/seesaw/src/runtime/handler_worker.rs` (lines 535-542) - Background handler emission

**Formula**:
```rust
event_id = Uuid::new_v5(
    &NAMESPACE_SEESAW,
    format!("{}-{}-{}-{}",
        parent_event_id,
        handler_id,
        event_type,
        emission_index
    ).as_bytes()
)
```

**Guarantees**:
- ✅ Same handler run multiple times → generates same event IDs
- ✅ Crash during handler execution → retry produces identical events
- ✅ Combined with `ON CONFLICT` → duplicate events silently ignored
- ✅ Deterministic timestamp (midnight UTC of parent day) → same partition

**Why this matters**: If a handler crashes after emitting events but before marking complete, a retry will regenerate the exact same event IDs, which will be rejected by the unique constraint.

---

### 3. Handler Execution Deduplication

**Mechanism**: Primary key constraint on (event_id, handler_id) pairs

**Files**:
- `docs/schema.sql` (lines 105-134) - `seesaw_handler_executions` table
- `crates/seesaw-postgres/src/lib.rs` (lines 328-354) - `poll_next_handler()` with locking
- `crates/seesaw/src/runtime/handler_worker.rs` (lines 82-478) - Handler worker processing

**Database schema** (from docs/schema.sql lines 105-134):
```sql
CREATE TABLE seesaw_handler_executions (
    event_id UUID NOT NULL,
    handler_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'pending',  -- pending, executing, completed, failed
    result JSONB,
    error TEXT,
    attempts INT NOT NULL DEFAULT 0,

    -- Event payload (survives 30-day retention deletion)
    event_type VARCHAR(255) NOT NULL,
    event_payload JSONB NOT NULL,    -- Copied from seesaw_events for delayed handlers
    parent_event_id UUID,
    batch_id UUID,
    batch_index INT,
    batch_size INT,

    -- Execution properties (from handler builder)
    execute_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- When to execute (.delay())
    timeout_seconds INT NOT NULL DEFAULT 30,
    max_attempts INT NOT NULL DEFAULT 3,
    priority INT NOT NULL DEFAULT 10,               -- Lower = higher priority
    join_window_timeout_seconds INT,

    claimed_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    last_attempted_at TIMESTAMPTZ,

    PRIMARY KEY (event_id, handler_id)  -- ← Prevents duplicate executions
);

-- Indexes for efficient worker polling
CREATE INDEX idx_handler_executions_pending
ON seesaw_handler_executions(priority ASC, execute_at ASC)
WHERE status = 'pending';

CREATE INDEX idx_handler_executions_retry
ON seesaw_handler_executions(priority ASC, execute_at ASC)
WHERE status = 'failed';
```

**Polling mechanism** (from seesaw-postgres/src/lib.rs lines 454-479):
```sql
-- Atomic claim with status transition using CTE
WITH next_effect AS (
    SELECT event_id, handler_id
    FROM seesaw_handler_executions
    WHERE (
        status = 'pending'
        OR (status = 'failed' AND attempts < max_attempts)
    )
      AND execute_at <= NOW()
    ORDER BY priority ASC, execute_at ASC, event_id ASC, handler_id ASC
    LIMIT 1
    FOR UPDATE SKIP LOCKED  -- ← Prevents race conditions
)
UPDATE seesaw_handler_executions e
SET status = 'executing',
    claimed_at = NOW(),
    last_attempted_at = NOW(),
    attempts = e.attempts + 1
FROM next_effect
WHERE e.event_id = next_effect.event_id
  AND e.handler_id = next_effect.handler_id
RETURNING *;
```

**Guarantees**:
- ✅ Only one execution record per (event, handler) tuple can exist
- ✅ `FOR UPDATE SKIP LOCKED` ensures only one worker claims a handler
- ✅ Status state machine prevents concurrent execution
- ✅ Retry increments `attempts` counter until max_attempts reached

---

### 4. Idempotency Keys for External APIs

**Mechanism**: Deterministic key generation from (event_id, handler_id)

**Files**:
- `crates/seesaw/src/handler/context.rs` (lines 22-139) - `HandlerContext` with `idempotency_key`
- `crates/seesaw/src/runtime/event_worker.rs` (lines 299-303) - Key generation (inline)
- `crates/seesaw/src/runtime/handler_worker.rs` (lines 187-191) - Key generation (background)

**Generation**:
```rust
let idempotency_key = Uuid::new_v5(
    &NAMESPACE_SEESAW,
    format!("{}-{}", event_id, handler_id).as_bytes()
).to_string();
```

**Usage pattern**:
```rust
handler::on::<OrderPlaced>()
    .extract(|e| e.order_id)
    .then(|order_id, ctx| async move {
        // Use idempotency key with external API (e.g., Stripe)
        let charge_id = ctx.deps().stripe.charge(
            order_id,
            ctx.idempotency_key()  // ← Same key on retry
        ).await?;

        Ok(PaymentCharged { order_id, charge_id })
    })
```

**Guarantees**:
- ✅ Same key across all retries of the same handler
- ✅ Stable even if handler crashes and is retried by different worker
- ✅ Per-handler scope (different handlers on same event have different keys)
- ⚠️ **Handler must use this key** - not automatic

**Critical**: This is a **tool provided to developers**, not automatic protection. Handlers that mutate external state must explicitly use `ctx.idempotency_key()`.

---

### 5. Retry and DLQ Handling

**Mechanism**: Exponential backoff with max attempts, DLQ for permanent failures

**Files**:
- `crates/seesaw-postgres/src/lib.rs` (lines 26-29) - Backoff calculation
- `crates/seesaw/src/runtime/handler_worker.rs` (lines 620-694) - `fail_or_dlq_handler()`
- `docs/schema.sql` (lines 210-236) - `seesaw_dlq` table

**Retry backoff**:
```rust
fn handler_retry_delay_seconds(attempts: i32) -> i64 {
    let exponent = attempts.saturating_sub(1).clamp(0, 8) as u32;
    1_i64 << exponent  // 1s, 2s, 4s, 8s, 16s, 32s, 64s, 128s, 256s
}
```

**Retry flow**:
```
pending → executing → failed (attempts < max_attempts)
                        ↓
                   [retry after backoff]
                        ↓
                   executing → ...

failed (attempts >= max_attempts) → DLQ
```

**DLQ terminal events**:
```rust
handler::on::<PaymentRequested>()
    .retry(3)
    .then(|event, ctx| async move {
        match ctx.deps().stripe.charge(&event).await {
            Ok(charge_id) => Ok(PaymentCharged { order_id: event.order_id, charge_id }),
            Err(e) if attempts >= 3 => Ok(PaymentFailed {
                order_id: event.order_id,
                reason: format!("Failed after 3 attempts: {}", e)
            }),
            Err(e) => Err(e.into())  // Retry
        }
    })
```

**Guarantees**:
- ✅ Exponential backoff prevents thundering herd
- ✅ Max attempts limit prevents infinite retries
- ✅ DLQ captures permanent failures for manual review
- ✅ Terminal events allow explicit failure handling
- ❌ **No automatic retry from DLQ** - requires manual intervention

---

### 6. Batch Processing and Join Windows

**Mechanism**: Append idempotency + atomic window closure

**Files**:
- `crates/seesaw-postgres/src/lib.rs` (lines 834-967) - Join window state machine
- `crates/seesaw/src/runtime/handler_worker.rs` (lines 245-268) - Join batch handling

**Append idempotency**:
```sql
INSERT INTO seesaw_join_entries (join_handler_id, correlation_id, source_event_id, ...)
VALUES ($1, $2, $3, ...)
ON CONFLICT (join_handler_id, correlation_id, source_event_id) DO NOTHING
```

**Window state machine**:
```
open → processing → completed
 ↑         ↓
 └─ (on retry/error)
```

**Guarantees**:
- ✅ Duplicate items won't re-append (ON CONFLICT)
- ✅ Window handler only runs once when closure condition met
- ✅ Retry releases window back to `open` but items already appended
- ✅ Atomic state transition prevents concurrent processing

---

### 7. QueueBackend Abstraction (v0.10.3+)

**Mechanism**: Pluggable queue transport while Store remains source of truth

**Files**:
- `crates/seesaw/src/queue_backend.rs` (lines 18-44) - `QueueBackend<St>` trait
- `crates/seesaw/src/queue_backend.rs` (lines 48-68) - `StoreQueueBackend` default implementation

**Trait definition**:
```rust
#[async_trait]
pub trait QueueBackend<St: Store>: Send + Sync + 'static {
    /// Human-readable backend name for diagnostics
    fn name(&self) -> &'static str { "custom" }

    /// Called after a handler intent is persisted in the store
    async fn on_effect_intent_inserted(
        &self,
        _store: &St,
        _event_id: Uuid,
        _effect_id: &str,  // ← Note: param name still "effect_id"
    ) -> Result<()> { Ok(()) }

    /// Fetch the next queued handler execution to process
    async fn poll_next_effect(&self, store: &St) -> Result<Option<QueuedHandlerExecution>> {
        store.poll_next_effect().await  // Delegate to store by default
    }
}
```

**Default implementation** (StoreQueueBackend):
```rust
pub struct StoreQueueBackend;

impl<St: Store> QueueBackend<St> for StoreQueueBackend {
    fn name(&self) -> &'static str { "store" }
    // Uses default trait implementations (delegates to store)
}
```

**Purpose**:
- **Store remains authoritative** - All handler state lives in database
- **Queue is notification layer** - External systems (Redis, Kafka) can wake workers
- **Deduplication still guaranteed** - Queue can lose messages, store won't duplicate work

**Use cases**:
- Redis pub/sub for low-latency worker wake-ups
- Kafka for high-throughput job distribution
- RabbitMQ for complex routing patterns
- Default: Poll store directly (simple, no external deps)

**Guarantees**:
- ✅ Queue messages can be lost - workers will still poll store
- ✅ Queue can deliver duplicates - handler execution PRIMARY KEY prevents re-execution
- ✅ Worker crash during polling - another worker claims via `FOR UPDATE SKIP LOCKED`

---

### 8. DistributedSafe Trait (v0.10.3+)

**Mechanism**: Compile-time safety for multi-worker deployments

**Files**:
- `crates/seesaw/src/distributed_safe.rs` - Sealed trait with explicit implementations

**Definition**:
```rust
/// Marker trait for types safe to use as dependencies in distributed workers.
/// Sealed trait - only framework-provided implementations allowed.
pub trait DistributedSafe: Send + Sync + 'static {}

// Implemented for:
impl DistributedSafe for String {}
impl DistributedSafe for sqlx::PgPool {}     // Database connections ✅
impl DistributedSafe for reqwest::Client {}  // HTTP clients ✅
impl DistributedSafe for redis::Client {}    // Redis clients ✅

// NOT implemented for:
// - Arc<Mutex<T>> (in-memory state fails across workers)
// - Arc<RwLock<T>> (local caches break distributed semantics)
```

**Purpose**:
- Prevents in-memory state in handler dependencies
- Compile error if you try to use `Arc<Mutex<HashMap<...>>>` as deps
- Forces externalized state (database, Redis, etc.)

**Guarantees**:
- ✅ Compile-time check - can't deploy handlers with local state
- ✅ Sealed trait - framework controls what's safe
- ✅ Feature-gated - only impls for deps you actually use

---

## What Seesaw Does NOT Guarantee

### Not Exactly-Once Semantics

Seesaw implements **at-most-once**, not exactly-once:

1. **Event processing is not transactional with handlers**
   - Events are processed and committed before handlers run
   - If handler crashes, event processing already completed
   - No automatic rollback

2. **External side effects are not guaranteed**
   - Handlers can call external APIs
   - Framework provides idempotency key but can't enforce usage
   - Partial failures (network issues) may leave inconsistent external state

3. **No automatic compensation**
   - Failed handlers can emit terminal events (manual)
   - No built-in saga/compensation patterns
   - Developer must design compensation logic

### Handler Responsibility

**Critical**: Handlers MUST:
- ✅ Use `ctx.idempotency_key()` for external API calls
- ✅ Design idempotent operations (check-then-mutate patterns)
- ✅ Handle partial failures gracefully
- ✅ Emit explicit failure events for compensation

---

## Key Files Reference

| File | Lines | Purpose |
|------|-------|---------|
| `docs/schema.sql` | Full | Database schema with all idempotency constraints (v0.9.1) |
| `crates/seesaw/src/store.rs` | 8-11 | Store trait + NAMESPACE_SEESAW constant |
| `crates/seesaw/src/handler/` | Full | Handler module (types, builders, context, error) |
| `crates/seesaw/src/handler/context.rs` | 8-77 | HandlerContext with idempotency_key |
| `crates/seesaw/src/handler/types.rs` | 150-211 | Handler<D> struct definition |
| `crates/seesaw-postgres/src/lib.rs` | 118-400 | PostgreSQL implementation of all deduplication logic |
| `crates/seesaw/src/runtime/event_worker.rs` | Full | Event processing + inline handler execution |
| `crates/seesaw/src/runtime/handler_worker.rs` | Full | Background handler execution + retry handling |
| `crates/seesaw/src/queue_backend.rs` | 18-68 | QueueBackend abstraction for pluggable queues (v0.10.3+) |
| `crates/seesaw/src/distributed_safe.rs` | Full | DistributedSafe trait for multi-worker safety (v0.10.3+) |
| `crates/seesaw-postgres/tests/integration_tests.rs` | Full | Test coverage for idempotency |

---

## Design Patterns

### Pattern 1: Idempotent External API Call

```rust
handler::on::<OrderPlaced>()
    .extract(|e| e.order_id)
    .retry(3)
    .then(|order_id, ctx| async move {
        // Use framework-provided idempotency key
        let charge = ctx.deps().stripe.charge_with_idempotency(
            order_id,
            ctx.idempotency_key()  // ← Stable across retries
        ).await?;

        Ok(PaymentCharged { order_id, charge_id: charge.id })
    })
```

### Pattern 2: Explicit Failure Events

```rust
handler::on::<PaymentRequested>()
    .extract(|e| e.order_id)
    .then(|order_id, ctx| async move {
        match ctx.deps().payment.charge(order_id).await {
            Ok(charge_id) => Ok(Emit::One(OrderEvent::PaymentCharged { order_id, charge_id })),
            Err(e) if e.is_retryable() => Err(e.into()),  // Retry via framework
            Err(e) => Ok(Emit::One(OrderEvent::PaymentFailed {  // Terminal failure
                order_id,
                reason: e.to_string()
            }))
        }
    })
```

### Pattern 3: Check-Then-Mutate Idempotency

```rust
handler::on::<UserCreated>()
    .extract(|e| e.user_id)
    .then(|user_id, ctx| async move {
        // Check if already sent (using idempotency key as external ID)
        if ctx.deps().email.was_sent(ctx.idempotency_key()).await? {
            return Ok(EmailSent { user_id, skipped: true });
        }

        // Send email
        ctx.deps().email.send(user_id, ctx.idempotency_key()).await?;
        Ok(EmailSent { user_id, skipped: false })
    })
```

---

## Summary: Deduplication Guarantees

| Level | Mechanism | Guarantee | Handler Responsibility |
|-------|-----------|-----------|----------------------|
| **Event Publishing** | `ON CONFLICT` + ledger | At-most-once insertion | None (automatic) |
| **Event ID Generation** | UUID v5 deterministic | Crash-safe idempotency | None (automatic) |
| **Handler Execution** | PRIMARY KEY + locking | One execution per (event, handler) | None (automatic) |
| **External API Calls** | Idempotency key provided | Stable key across retries | **Must use key** |
| **Event Emission** | Deterministic IDs + `ON CONFLICT` | Duplicate events rejected | None (automatic) |
| **Retry Handling** | Exponential backoff + max attempts | Bounded retries | Design idempotent handlers |
| **Batch Joins** | `ON CONFLICT` + state machine | Single window processing | None (automatic) |
| **Queue Backend** | Store as source of truth | Queue can lose/duplicate messages | None (automatic) |
| **Distributed Workers** | `DistributedSafe` trait | No in-memory state (compile-time) | Use trait-marked deps |

**Key Insight**: Seesaw provides robust infrastructure for deduplication, but handlers must be designed with idempotency in mind. The framework gives you `ctx.idempotency_key()` - you must use it.

---

## What Was Removed in v0.10.3

To understand the current architecture, note what was **removed** during the Effect→Handler refactor:

**Removed Concepts**:
- ✂️ **Reducers** - State transformations before handlers (simplified to handler-only model)
- ✂️ **Task Groups** - `task_group.rs` module (replaced with correlation_id tracking)
- ✂️ **Transition predicates** - `.transition()` builder method (use `.filter()` instead)
- ✂️ **Multiple execution modes** - Simplified to inline + background (queued) only

**Why removed**:
- Reducers added complexity with minimal benefit (handlers can query/update state)
- Task groups were over-engineered for correlation tracking
- Simpler API surface = easier to understand and maintain

**What remains**:
- ✅ Events as immutable facts
- ✅ Handlers that react and emit new events
- ✅ Correlation IDs for workflow tracking
- ✅ All deduplication guarantees intact
