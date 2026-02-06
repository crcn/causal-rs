# Distributed Safety Guide

**Last Updated:** 2026-02-06

This guide explains how to write Seesaw handlers that are safe for multi-worker distributed deployments.

## Quick Start

### ✅ Safe Pattern

```rust
use seesaw_core::{Engine, DistributedSafe};
use sqlx::PgPool;

// Safe for distributed workers
#[derive(Clone, DistributedSafe)]
struct Deps {
    db: PgPool,  // ✅ Shared across workers
}

let engine = Engine::new(deps, PostgresStore::new(pool))
    .with_handler(handler::on::<OrderPlaced>().then(|event, ctx| async move {
        // All workers share same database
        sqlx::query("INSERT INTO orders ...")
            .execute(&ctx.deps().db)
            .await?;
        Ok(OrderSaved { order_id: event.order_id })
    }));
```

### ❌ Unsafe Pattern (Compile Error)

```rust
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

// ❌ Compile error: Arc<Mutex> not DistributedSafe
#[derive(Clone, DistributedSafe)]
struct BadDeps {
    db: PgPool,
    cache: Arc<Mutex<HashMap<Uuid, User>>>,  // ❌ ERROR
}

// Error: field type may not be distributed-safe (contains Arc<Mutex> or similar).
//        Either: (1) use external storage (Database, Redis),
//                (2) use event-threaded state, or
//                (3) add #[allow_non_distributed] attribute to explicitly opt-out
```

## The Problem

When you run multiple Seesaw workers as separate processes:

```
Worker 1 starts → Deps { cache: {} }
Worker 2 starts → Deps { cache: {} }
Worker 3 starts → Deps { cache: {} }

Worker 1: cache[order_123] = "Shipped"   → cache = { order_123: "Shipped" }
Worker 2: cache[order_123] = "Pending"   → cache = { order_123: "Pending" }
Worker 3: cache[order_456] = "Delivered" → cache = { order_456: "Delivered" }

Each worker has different data! ☠️
```

**The workers never see each other's updates** because `Arc<Mutex>` only works within a single process.

## Safe State Management Patterns

### Pattern 1: State in Events (Recommended)

State flows through event fields. Fully auditable and distributed-safe.

```rust
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Processing {
        order_id: Uuid,
        items_processed: usize,    // State flows through events
        items_remaining: Vec<Item>,
    },
    Complete {
        order_id: Uuid,
        total_items: usize,
    },
}

#[handler(on = OrderEvent, extract(order_id, items_processed, items_remaining))]
async fn process_order(
    order_id: Uuid,
    items_processed: usize,
    items_remaining: Vec<Item>,
    ctx: HandlerContext<Deps>
) -> Result<OrderEvent> {
    if let Some((item, rest)) = items_remaining.split_first() {
        ctx.deps().process_item(item).await?;
        Ok(OrderEvent::Processing {
            order_id,
            items_processed: items_processed + 1,
            items_remaining: rest.to_vec(),
        })
    } else {
        Ok(OrderEvent::Complete {
            order_id,
            total_items: items_processed,
        })
    }
}
```

**Why this works:** Events are persisted in Postgres. All workers see the same events.

### Pattern 2: External Storage

State stored in external systems that all workers can access.

```rust
use sqlx::PgPool;
use redis::Client as RedisClient;

#[derive(Clone, DistributedSafe)]
struct Deps {
    db: PgPool,          // ✅ All workers share same Postgres
    redis: RedisClient,  // ✅ All workers share same Redis
}

#[handler(on = OrderPlaced)]
async fn save_order(event: OrderPlaced, ctx: HandlerContext<Deps>) -> Result<OrderSaved> {
    // State in database - all workers see it
    sqlx::query("INSERT INTO orders (id, status) VALUES ($1, 'placed')")
        .bind(event.order_id)
        .execute(&ctx.deps().db)
        .await?;
    Ok(OrderSaved { order_id: event.order_id })
}

#[handler(on = OrderShipped)]
async fn update_cache(event: OrderShipped, ctx: HandlerContext<Deps>) -> Result<()> {
    // Cache in Redis - all workers share it
    let mut conn = ctx.deps().redis.get_async_connection().await?;
    redis::cmd("SET")
        .arg(format!("order:{}", event.order_id))
        .arg("shipped")
        .query_async(&mut conn)
        .await?;
    Ok(())
}
```

**Why this works:** External systems (Postgres, Redis) are shared across all workers.

### Pattern 3: Implicit State

State is implicit in "which events have fired".

```rust
// State machine:
// OrderPlaced → "placed"
// OrderShipped → "shipped"
// OrderDelivered → "delivered"

#[handler(on = OrderPlaced)]
async fn ship_order(event: OrderPlaced, ctx: Ctx) -> Result<OrderShipped> {
    ctx.deps().ship(event.order_id).await?;
    Ok(OrderShipped { order_id: event.order_id })
}

#[handler(on = OrderShipped)]
async fn deliver_order(event: OrderShipped, ctx: Ctx) -> Result<OrderDelivered> {
    ctx.deps().deliver(event.order_id).await?;
    Ok(OrderDelivered { order_id: event.order_id })
}
```

**Why this works:** Events are persisted. All workers see the same event history.

## Single-Process Opt-Out

For development or single-worker deployments, you can explicitly opt out:

```rust
#[derive(Clone, DistributedSafe)]
struct DevDeps {
    db: PgPool,

    #[allow_non_distributed]  // ⚠️ Explicit warning
    cache: Arc<Mutex<HashMap<Uuid, User>>>,
}
```

**Only use this when:**
- ✅ Running exactly one worker (development, testing)
- ✅ You'll never scale horizontally
- ❌ **NEVER in production with multiple workers**

## Execution Modes

Understanding execution modes is critical for correctness.

### Inline Handlers (Default)

Fast, atomic with event dispatch:

```rust
// Inline - runs immediately
#[handler(on = OrderPlaced)]
async fn save_order(event: OrderPlaced, ctx: Ctx) -> Result<OrderSaved> {
    ctx.deps().db.insert_order(&event).await?;
    Ok(OrderSaved { order_id: event.order_id })
}

// Execution: [TX begins] → Insert OrderPlaced → Handler runs → Insert OrderSaved → [TX commits]
```

**Use when:**
- Fast operations (<100ms)
- Must be atomic with dispatch
- Database updates

### Background Handlers (Queued)

Async, retryable, distributed across workers:

```rust
// Background - queued for workers
#[handler(on = PaymentRequested, queued, retry = 3, timeout_secs = 30)]
async fn charge_payment(event: PaymentRequested, ctx: Ctx) -> Result<PaymentCharged> {
    ctx.deps().stripe.charge(&event).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}

// Execution: [TX A: Insert event + handler intent] → Worker picks up → [TX B: Handler runs]
```

**Use when:**
- Slow operations (external APIs)
- Need retry on failure
- Can be asynchronous

**Key difference:** Inline = same transaction. Background = separate transaction.

## Common Mistakes

### Mistake 1: Assuming Shared Memory

```rust
// ❌ BAD: Assumes workers share memory
#[derive(Clone)]
struct Deps {
    counter: Arc<Mutex<i32>>,
}

#[handler(on = Event)]
async fn increment(event: Event, ctx: Ctx) -> Result<()> {
    let mut count = ctx.deps().counter.lock().unwrap();
    *count += 1;
    println!("Count: {}", *count);  // Different on each worker!
    Ok(())
}
```

**Fix:** Use database or Redis for shared counters.

```rust
// ✅ GOOD: Counter in database
#[handler(on = Event)]
async fn increment(event: Event, ctx: Ctx) -> Result<()> {
    sqlx::query("UPDATE counters SET count = count + 1 WHERE id = 'global'")
        .execute(&ctx.deps().db)
        .await?;
    Ok(())
}
```

### Mistake 2: Caching Without Invalidation

```rust
// ❌ BAD: Local cache per worker
#[derive(Clone)]
struct Deps {
    db: PgPool,
    cache: Arc<Mutex<HashMap<Uuid, User>>>,
}

#[handler(on = UserUpdated)]
async fn update_user(event: UserUpdated, ctx: Ctx) -> Result<()> {
    // Update database (all workers see)
    sqlx::query("UPDATE users SET name = $1").execute(&ctx.deps().db).await?;

    // Update cache (only this worker sees!)
    ctx.deps().cache.lock().unwrap().insert(event.user_id, event.user);

    Ok(())
}
// Other workers still have old cached data! ☠️
```

**Fix:** Use Redis for shared cache.

```rust
// ✅ GOOD: Shared Redis cache
#[derive(Clone, DistributedSafe)]
struct Deps {
    db: PgPool,
    redis: RedisClient,
}

#[handler(on = UserUpdated)]
async fn update_user(event: UserUpdated, ctx: Ctx) -> Result<()> {
    // Update database
    sqlx::query("UPDATE users SET name = $1").execute(&ctx.deps().db).await?;

    // Update shared cache (all workers see)
    redis::cmd("SET")
        .arg(format!("user:{}", event.user_id))
        .arg(serde_json::to_string(&event.user)?)
        .query_async(&mut ctx.deps().redis.get_async_connection().await?)
        .await?;

    Ok(())
}
```

### Mistake 3: Mixing Transactions

```rust
// ❌ BAD: Assuming inline + background are atomic
#[handler(on = OrderPlaced)]
async fn save_order_inline(event: OrderPlaced, ctx: Ctx) -> Result<PaymentRequested> {
    sqlx::query("INSERT INTO orders ...").execute(&ctx.deps().db).await?;
    Ok(PaymentRequested { order_id: event.order_id })
}

#[handler(on = PaymentRequested, queued, retry = 3)]
async fn charge_payment_background(event: PaymentRequested, ctx: Ctx) -> Result<PaymentCharged> {
    ctx.deps().stripe.charge(&event).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}

// Problem: If charge fails, order is already committed! Not atomic.
```

**Fix:** Make both idempotent and handle failures explicitly.

```rust
// ✅ GOOD: Idempotent with explicit failure handling
#[handler(on = PaymentRequested, queued, retry = 3)]
async fn charge_payment(event: PaymentRequested, ctx: Ctx) -> Result<PaymentResult> {
    match ctx.deps().stripe.charge(&event).await {
        Ok(_) => Ok(PaymentResult::Charged { order_id: event.order_id }),
        Err(e) => Ok(PaymentResult::Failed {
            order_id: event.order_id,
            reason: e.to_string()
        }),
    }
}

#[handler(on = PaymentResult)]
async fn handle_payment_result(event: PaymentResult, ctx: Ctx) -> Result<()> {
    match event {
        PaymentResult::Charged { order_id } => {
            sqlx::query("UPDATE orders SET status = 'paid'").execute(&ctx.deps().db).await?;
        }
        PaymentResult::Failed { order_id, reason } => {
            sqlx::query("UPDATE orders SET status = 'payment_failed'").execute(&ctx.deps().db).await?;
        }
    }
    Ok(())
}
```

## Summary

| Pattern | Multi-Worker | Audit Trail | Complexity |
|---------|--------------|-------------|------------|
| **State in Events** | ✅ | ✅ | Low-Medium |
| **External Storage (DB/Redis)** | ✅ | ⚠️ | Medium |
| **Implicit State** | ✅ | ✅ | Low |
| **Arc<Mutex> (single-process)** | ❌ | ❌ | Low |

**For production systems:** Use the first three patterns. Avoid `Arc<Mutex>` unless you have exactly one worker.

**The `DistributedSafe` trait prevents this at compile time.**
