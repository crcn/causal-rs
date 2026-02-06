# CQRS Compatibility with Seesaw

**Date:** 2026-02-06

## TL;DR

**YES, Seesaw is highly compatible with CQRS.** In fact, Seesaw is the event-driven backbone that sits between the command side and query side.

```
CQRS Pattern:
  Command → Write Model → Events → Read Model(s)

Seesaw's Role:
  Command → [Thin Layer] → Seesaw Events → Handlers Update Read Models
                             ↓
                        Event Store (source of truth)
```

**Key insight:** Seesaw handles the "Events → Read Models" part of CQRS. Commands can be a thin wrapper on top.

---

## Part 1: CQRS Fundamentals

### What is CQRS?

**Command Query Responsibility Segregation** - Separate write models from read models.

**Core principles:**
1. **Commands** - Change state (writes)
2. **Queries** - Return data (reads)
3. **Separation** - Different models for writes vs reads
4. **Events** - Commands produce events, events update read models

### Traditional CQRS Architecture

```
┌─────────────┐
│   Client    │
└─────┬───────┘
      │
      ├─────────────────┐
      │                 │
      ▼                 ▼
┌──────────┐      ┌──────────┐
│ Commands │      │ Queries  │
└────┬─────┘      └────┬─────┘
     │                 │
     ▼                 ▼
┌──────────┐      ┌──────────┐
│  Write   │      │   Read   │
│  Model   │      │  Model   │
└────┬─────┘      └────▲─────┘
     │                 │
     │    ┌────────┐   │
     └───►│ Events │───┘
          └────────┘
```

**Flow:**
1. Client sends command
2. Command handler validates + executes on write model
3. Write model produces events
4. Events stored in event store
5. Event handlers update read models (projections)
6. Client queries read models

---

## Part 2: Seesaw's Role in CQRS

### Seesaw as the Event Backbone

**Seesaw handles:**
- ✅ Event persistence (event store)
- ✅ Event dispatch (pub/sub)
- ✅ Event handlers (projection builders)
- ✅ Retry logic (at-least-once delivery)
- ✅ Workflow orchestration (multi-step processes)

**Seesaw does NOT handle:**
- ❌ Command validation (you do this)
- ❌ Command routing (you do this)
- ❌ Query execution (you do this)

### Architecture with Seesaw

```
┌─────────────┐
│   Client    │
└─────┬───────┘
      │
      ├─────────────────────────┐
      │                         │
      ▼                         ▼
┌─────────────┐         ┌────────────┐
│  Command    │         │   Query    │
│  Handler    │         │  Handler   │
│  (Your Code)│         │ (Your Code)│
└──────┬──────┘         └──────┬─────┘
       │                       │
       │                       │
       ▼                       ▼
┌─────────────────────────────────────┐
│            SEESAW                   │
│  ┌──────────────────────────────┐  │
│  │      Event Store             │  │
│  │  (seesaw_events table)       │  │
│  └──────────────────────────────┘  │
│               │                     │
│               ▼                     │
│  ┌──────────────────────────────┐  │
│  │     Event Handlers           │  │
│  │  - Update Read Model 1       │  │
│  │  - Update Read Model 2       │  │
│  │  - Update Analytics          │  │
│  │  - Send Notifications        │  │
│  └──────────────────────────────┘  │
└─────────────────────────────────────┘
                │
                ▼
         ┌─────────────┐
         │ Read Models │
         │  (Tables)   │
         └─────────────┘
```

---

## Part 3: Implementation Patterns

### Pattern 1: Commands as Event Dispatches (Simple)

**Use when:** Commands are simple, validation is light

```rust
// Command (just a request)
#[derive(Debug)]
struct CreateOrder {
    user_id: Uuid,
    items: Vec<Item>,
    total: f64,
}

// Command handler dispatches event directly
async fn handle_create_order(
    cmd: CreateOrder,
    engine: &Engine,
) -> Result<Uuid> {
    // Validate
    if cmd.items.is_empty() {
        bail!("Order must have items");
    }

    // Generate ID
    let order_id = Uuid::new_v4();

    // Dispatch event (write to event store)
    engine.dispatch(OrderPlaced {
        order_id,
        user_id: cmd.user_id,
        items: cmd.items,
        total: cmd.total,
    }).await?;

    Ok(order_id)
}

// Seesaw handlers update read models
#[handler(on = OrderPlaced)]
async fn update_orders_view(
    event: OrderPlaced,
    ctx: Ctx,
) -> Result<()> {
    // Update denormalized "orders" read model
    sqlx::query(
        "INSERT INTO orders_view (id, user_id, total, status)
         VALUES ($1, $2, $3, 'placed')"
    )
    .bind(event.order_id)
    .bind(event.user_id)
    .bind(event.total)
    .execute(&ctx.deps().db)
    .await?;

    Ok(())
}

#[handler(on = OrderPlaced)]
async fn update_user_stats_view(
    event: OrderPlaced,
    ctx: Ctx,
) -> Result<()> {
    // Update "user_stats" read model
    sqlx::query(
        "UPDATE user_stats
         SET total_orders = total_orders + 1,
             total_spent = total_spent + $2
         WHERE user_id = $1"
    )
    .bind(event.user_id)
    .bind(event.total)
    .execute(&ctx.deps().db)
    .await?;

    Ok(())
}

// Query handler reads from read model (outside Seesaw)
async fn get_user_orders(user_id: Uuid, db: &PgPool) -> Result<Vec<Order>> {
    sqlx::query_as(
        "SELECT * FROM orders_view WHERE user_id = $1 ORDER BY created_at DESC"
    )
    .bind(user_id)
    .fetch_all(db)
    .await
}
```

**Characteristics:**
- ✅ Simple - commands are thin wrappers
- ✅ Fast - no separate write model
- ⚠️ Write model = event fields (may be denormalized)

---

### Pattern 2: Commands with Rich Write Model (Traditional CQRS)

**Use when:** Complex validation, aggregates, business logic

```rust
// Write Model (aggregate)
struct Order {
    id: Uuid,
    user_id: Uuid,
    items: Vec<Item>,
    status: OrderStatus,
}

impl Order {
    // Factory method (command handler)
    async fn create(
        cmd: CreateOrder,
        db: &PgPool,
    ) -> Result<Vec<Event>> {
        // Complex validation
        Self::validate_user_limits(&cmd.user_id, db).await?;
        Self::validate_inventory(&cmd.items, db).await?;

        let order_id = Uuid::new_v4();

        // Business logic
        let total = cmd.items.iter().map(|i| i.price).sum();

        // Return events (to be dispatched via Seesaw)
        Ok(vec![
            Event::OrderPlaced {
                order_id,
                user_id: cmd.user_id,
                items: cmd.items,
                total,
            }
        ])
    }

    // Command handler
    async fn cancel(
        order_id: Uuid,
        reason: String,
        db: &PgPool,
    ) -> Result<Vec<Event>> {
        // Load aggregate state from event store
        let order = Self::load_from_events(order_id, db).await?;

        // Validate state transition
        if !order.can_cancel() {
            bail!("Order cannot be cancelled in status: {:?}", order.status);
        }

        // Return events
        Ok(vec![
            Event::OrderCancelled {
                order_id,
                reason,
            }
        ])
    }

    // Reconstitute aggregate from events (event sourcing)
    async fn load_from_events(
        order_id: Uuid,
        db: &PgPool,
    ) -> Result<Self> {
        let events = sqlx::query_as::<_, StoredEvent>(
            "SELECT * FROM seesaw_events
             WHERE payload->>'order_id' = $1
             ORDER BY created_at"
        )
        .bind(order_id.to_string())
        .fetch_all(db)
        .await?;

        let mut order = None;

        for event in events {
            match event.event_type.as_str() {
                "OrderPlaced" => {
                    let e: OrderPlaced = serde_json::from_value(event.payload)?;
                    order = Some(Order {
                        id: e.order_id,
                        user_id: e.user_id,
                        items: e.items,
                        status: OrderStatus::Placed,
                    });
                }
                "OrderCancelled" => {
                    if let Some(ref mut o) = order {
                        o.status = OrderStatus::Cancelled;
                    }
                }
                _ => {}
            }
        }

        order.ok_or_else(|| anyhow!("Order not found"))
    }
}

// API endpoint (command bus)
#[post("/orders")]
async fn create_order_endpoint(
    cmd: Json<CreateOrder>,
    engine: Data<Engine>,
    db: Data<PgPool>,
) -> Result<Json<Uuid>> {
    // Execute command on aggregate
    let events = Order::create(cmd.into_inner(), &db).await?;

    // Dispatch events via Seesaw
    for event in events {
        engine.dispatch(event).await?;
    }

    Ok(Json(order_id))
}

// Seesaw handlers update read models (same as Pattern 1)
#[handler(on = OrderPlaced)]
async fn update_orders_view(...) { ... }
```

**Characteristics:**
- ✅ Rich domain model (aggregates)
- ✅ Complex validation
- ✅ Event sourcing (load from events)
- ⚠️ More code (aggregate logic)

---

### Pattern 3: Eventual Consistency with Multiple Read Models

**Use when:** Different query requirements, optimization

```rust
// Single event updates multiple read models
#[handler(on = OrderPlaced)]
async fn update_orders_view(event: OrderPlaced, ctx: Ctx) -> Result<()> {
    // Normalized read model (for order details)
    sqlx::query(
        "INSERT INTO orders_view (id, user_id, total, items_json)
         VALUES ($1, $2, $3, $4)"
    )
    .bind(event.order_id)
    .bind(event.user_id)
    .bind(event.total)
    .bind(serde_json::to_value(&event.items)?)
    .execute(&ctx.deps().db)
    .await?;

    Ok(())
}

#[handler(on = OrderPlaced)]
async fn update_user_stats_view(event: OrderPlaced, ctx: Ctx) -> Result<()> {
    // Denormalized read model (for user dashboard)
    sqlx::query(
        "INSERT INTO user_orders_summary (user_id, order_id, total, created_at)
         VALUES ($1, $2, $3, NOW())
         ON CONFLICT (user_id) DO UPDATE
         SET total_orders = user_orders_summary.total_orders + 1,
             total_spent = user_orders_summary.total_spent + $3"
    )
    .bind(event.user_id)
    .bind(event.order_id)
    .bind(event.total)
    .execute(&ctx.deps().db)
    .await?;

    Ok(())
}

#[handler(on = OrderPlaced)]
async fn update_analytics_view(event: OrderPlaced, ctx: Ctx) -> Result<()> {
    // Analytics read model (for reporting)
    sqlx::query(
        "INSERT INTO daily_sales (date, order_count, revenue)
         VALUES (CURRENT_DATE, 1, $1)
         ON CONFLICT (date) DO UPDATE
         SET order_count = daily_sales.order_count + 1,
             revenue = daily_sales.revenue + $1"
    )
    .bind(event.total)
    .execute(&ctx.deps().db)
    .await?;

    Ok(())
}

#[handler(on = OrderPlaced)]
async fn update_elasticsearch(event: OrderPlaced, ctx: Ctx) -> Result<()> {
    // Search read model (for full-text search)
    ctx.deps().elasticsearch
        .index("orders")
        .id(event.order_id)
        .body(&event)
        .send()
        .await?;

    Ok(())
}

// Different queries use different read models
async fn get_order_details(order_id: Uuid, db: &PgPool) -> Result<Order> {
    // Uses orders_view (normalized)
    sqlx::query_as("SELECT * FROM orders_view WHERE id = $1")
        .bind(order_id)
        .fetch_one(db)
        .await
}

async fn get_user_dashboard(user_id: Uuid, db: &PgPool) -> Result<UserDashboard> {
    // Uses user_orders_summary (denormalized)
    sqlx::query_as("SELECT * FROM user_orders_summary WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(db)
        .await
}

async fn search_orders(query: &str, es: &Elasticsearch) -> Result<Vec<Order>> {
    // Uses Elasticsearch (full-text search)
    es.search(SearchParts::Index(&["orders"]))
        .body(json!({ "query": { "match": { "_all": query } } }))
        .send()
        .await
}
```

**Characteristics:**
- ✅ Optimized read models per query type
- ✅ Multiple storage backends (Postgres, Elasticsearch, Redis)
- ✅ Eventually consistent (handlers run async)
- ⚠️ More complexity (maintain multiple models)

---

## Part 4: CQRS Benefits with Seesaw

### Benefit 1: Independent Read Model Scaling

**OLD (coupled read/write):**
```rust
// Single transaction for write + read model update
async fn create_order(cmd: CreateOrder) -> Result<Uuid> {
    let mut tx = db.begin().await?;

    // Write
    let order_id = insert_order(&cmd, &mut tx).await?;

    // Update read models (blocks write!)
    update_orders_view(&order_id, &mut tx).await?;
    update_user_stats(&cmd.user_id, &mut tx).await?;
    update_elasticsearch(&order_id).await?; // Slow!

    tx.commit().await?;  // Waits for everything
    Ok(order_id)
}
```

**NEW (decoupled with Seesaw):**
```rust
// Write (fast)
async fn create_order(cmd: CreateOrder, engine: &Engine) -> Result<Uuid> {
    let order_id = Uuid::new_v4();

    engine.dispatch(OrderPlaced { order_id, ... }).await?;
    // Returns immediately after event persisted (<10ms)

    Ok(order_id)
}

// Read model updates (async, independent)
#[handler(on = OrderPlaced)]
async fn update_orders_view(...) { /* Fast */ }

#[handler(on = OrderPlaced)]
async fn update_elasticsearch(...) { /* Slow, doesn't block write */ }
```

**Result:**
- ✅ Write latency: <10ms (not blocked by read model updates)
- ✅ Read models update independently
- ✅ Slow read models (Elasticsearch) don't block fast ones (Postgres)

---

### Benefit 2: Multiple Read Models from Single Event Stream

```rust
// Single event stream (source of truth)
Events: OrderPlaced → OrderShipped → OrderDelivered

// Multiple projections
Projection 1: orders_view (current state)
  OrderPlaced  → INSERT
  OrderShipped → UPDATE status = 'shipped'
  OrderDelivered → UPDATE status = 'delivered'

Projection 2: order_history (audit trail)
  OrderPlaced    → INSERT event
  OrderShipped   → INSERT event
  OrderDelivered → INSERT event

Projection 3: analytics (aggregates)
  OrderPlaced → Increment daily_orders
  OrderShipped → Update shipping_times
  OrderDelivered → Update delivery_times

Projection 4: elasticsearch (search)
  OrderPlaced    → Index document
  OrderShipped   → Update document
  OrderDelivered → Update document
```

**Each projection can:**
- Use different storage (Postgres, Elasticsearch, Redis)
- Have different schema (normalized vs denormalized)
- Update at different speeds (fast vs slow)
- Retry independently (Seesaw handles this)

---

### Benefit 3: Rebuild Read Models (Replay)

**CQRS benefit: Can rebuild read models by replaying events**

```rust
// Rebuild orders_view from event store
async fn rebuild_orders_view(db: &PgPool) -> Result<()> {
    // Clear existing read model
    sqlx::query("TRUNCATE orders_view").execute(db).await?;

    // Replay all events
    let events = sqlx::query_as::<_, StoredEvent>(
        "SELECT * FROM seesaw_events
         WHERE event_type IN ('OrderPlaced', 'OrderShipped', 'OrderDelivered')
         ORDER BY created_at"
    )
    .fetch_all(db)
    .await?;

    for event in events {
        match event.event_type.as_str() {
            "OrderPlaced" => {
                let e: OrderPlaced = serde_json::from_value(event.payload)?;
                // Re-run projection logic
                sqlx::query(
                    "INSERT INTO orders_view (id, user_id, total, status)
                     VALUES ($1, $2, $3, 'placed')"
                )
                .bind(e.order_id)
                .bind(e.user_id)
                .bind(e.total)
                .execute(db)
                .await?;
            }
            "OrderShipped" => { /* ... */ }
            "OrderDelivered" => { /* ... */ }
            _ => {}
        }
    }

    Ok(())
}
```

**Use cases:**
- Fix bugs in projection logic (rebuild with corrected code)
- Add new read models (replay events to populate)
- Migrate schema (rebuild with new schema)
- Recover from corruption (rebuild from source of truth)

---

### Benefit 4: Temporal Queries (Point-in-Time)

**CQRS + Event Sourcing: Can query state at any point in time**

```rust
// Get order state as of specific date
async fn get_order_at_time(
    order_id: Uuid,
    at_time: DateTime<Utc>,
    db: &PgPool,
) -> Result<Order> {
    // Query events up to that time
    let events = sqlx::query_as::<_, StoredEvent>(
        "SELECT * FROM seesaw_events
         WHERE payload->>'order_id' = $1
           AND created_at <= $2
         ORDER BY created_at"
    )
    .bind(order_id.to_string())
    .bind(at_time)
    .fetch_all(db)
    .await?;

    // Reconstruct state by replaying events
    let mut order = None;

    for event in events {
        match event.event_type.as_str() {
            "OrderPlaced" => {
                let e: OrderPlaced = serde_json::from_value(event.payload)?;
                order = Some(Order {
                    id: e.order_id,
                    status: OrderStatus::Placed,
                    // ...
                });
            }
            "OrderCancelled" => {
                if let Some(ref mut o) = order {
                    o.status = OrderStatus::Cancelled;
                }
            }
            _ => {}
        }
    }

    order.ok_or_else(|| anyhow!("Order not found"))
}

// Use case: "What was the order status on January 15th?"
let order = get_order_at_time(
    order_id,
    DateTime::parse_from_rfc3339("2025-01-15T00:00:00Z")?,
    &db,
).await?;
```

---

## Part 5: CQRS with New Async-First Architecture

### How Async-First Improves CQRS

**OLD (inline handlers):**
```
Command → Event persisted → Read models updated in same TX
          ↑_______________________________________________↑
          Single long transaction (slow read models block write)
```

**NEW (async handlers):**
```
Command → Event persisted (fast!)
            ↓
          Queue (separate streams per event type)
            ↓
          Read model handlers (independent, async)
```

### Performance Improvement

**Scenario: Order placement with 5 read models**

**OLD (inline):**
```
Write: 10ms (insert order)
Read model 1: 20ms (Postgres update)
Read model 2: 30ms (Elasticsearch index)
Read model 3: 15ms (Redis update)
Read model 4: 50ms (Analytics aggregate)
Read model 5: 100ms (External webhook)

Total: 225ms (all sequential, in same transaction!)
```

**NEW (async):**
```
Write: 10ms (persist event)
Command returns immediately ✅

Read models update independently:
- Model 1: 20ms (completes at T+20ms)
- Model 2: 30ms (completes at T+30ms)
- Model 3: 15ms (completes at T+15ms)
- Model 4: 50ms (completes at T+50ms)
- Model 5: 100ms (completes at T+100ms)

User sees response: 10ms (22.5x faster!)
All read models updated by: T+100ms (eventual consistency)
```

### Separate Streams for Different Read Models

**With new architecture:**
```
OrderPlaced event →
  ├─ "seesaw.OrderPlaced" stream →
  │    ├─ Fast read models (Postgres) - Process immediately
  │    ├─ Slow read models (Elasticsearch) - Process when ready
  │    └─ External systems (webhooks) - Process with retry
  │
  └─ Each handler is independent, can fail/retry separately
```

**Benefits:**
- ✅ Fast read models don't wait for slow ones
- ✅ External system failures don't break internal models
- ✅ Can scale workers per read model type
- ✅ Natural backpressure (slow models queue up)

---

## Part 6: Complete CQRS Example

### E-Commerce Order System

```rust
// ============================================================================
// COMMAND SIDE
// ============================================================================

// Commands (requests to change state)
#[derive(Debug)]
struct CreateOrder {
    user_id: Uuid,
    items: Vec<Item>,
}

#[derive(Debug)]
struct CancelOrder {
    order_id: Uuid,
    reason: String,
}

// Command handlers (validate + dispatch events)
async fn handle_create_order(
    cmd: CreateOrder,
    engine: &Engine,
    db: &PgPool,
) -> Result<Uuid> {
    // Validation
    if cmd.items.is_empty() {
        bail!("Order must have items");
    }

    // Check user credit limit
    let user = get_user(cmd.user_id, db).await?;
    let total: f64 = cmd.items.iter().map(|i| i.price).sum();

    if total > user.credit_limit {
        bail!("Exceeds credit limit");
    }

    // Generate ID
    let order_id = Uuid::new_v4();

    // Dispatch event (write to event store)
    engine.dispatch(OrderPlaced {
        order_id,
        user_id: cmd.user_id,
        items: cmd.items,
        total,
    }).await?;

    Ok(order_id)
}

async fn handle_cancel_order(
    cmd: CancelOrder,
    engine: &Engine,
    db: &PgPool,
) -> Result<()> {
    // Load current state from read model (or event store)
    let order = get_order(cmd.order_id, db).await?;

    // Validate state transition
    if !order.can_cancel() {
        bail!("Order cannot be cancelled in status: {:?}", order.status);
    }

    // Dispatch event
    engine.dispatch(OrderCancelled {
        order_id: cmd.order_id,
        reason: cmd.reason,
    }).await?;

    Ok(())
}

// ============================================================================
// EVENTS (Source of Truth)
// ============================================================================

#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    OrderPlaced {
        order_id: Uuid,
        user_id: Uuid,
        items: Vec<Item>,
        total: f64,
    },
    OrderShipped {
        order_id: Uuid,
        tracking_number: String,
    },
    OrderDelivered {
        order_id: Uuid,
        delivered_at: DateTime<Utc>,
    },
    OrderCancelled {
        order_id: Uuid,
        reason: String,
    },
}

// ============================================================================
// QUERY SIDE - Read Model 1: Orders View (Current State)
// ============================================================================

#[handler(on = OrderEvent)]
async fn update_orders_view(
    event: OrderEvent,
    ctx: Ctx,
) -> Result<()> {
    match event {
        OrderEvent::OrderPlaced { order_id, user_id, items, total } => {
            sqlx::query(
                "INSERT INTO orders_view
                 (id, user_id, total, items_json, status, created_at)
                 VALUES ($1, $2, $3, $4, 'placed', NOW())"
            )
            .bind(order_id)
            .bind(user_id)
            .bind(total)
            .bind(serde_json::to_value(&items)?)
            .execute(&ctx.deps().db)
            .await?;
        }
        OrderEvent::OrderShipped { order_id, tracking_number } => {
            sqlx::query(
                "UPDATE orders_view
                 SET status = 'shipped',
                     tracking_number = $2,
                     shipped_at = NOW()
                 WHERE id = $1"
            )
            .bind(order_id)
            .bind(tracking_number)
            .execute(&ctx.deps().db)
            .await?;
        }
        OrderEvent::OrderDelivered { order_id, delivered_at } => {
            sqlx::query(
                "UPDATE orders_view
                 SET status = 'delivered',
                     delivered_at = $2
                 WHERE id = $1"
            )
            .bind(order_id)
            .bind(delivered_at)
            .execute(&ctx.deps().db)
            .await?;
        }
        OrderEvent::OrderCancelled { order_id, reason } => {
            sqlx::query(
                "UPDATE orders_view
                 SET status = 'cancelled',
                     cancel_reason = $2,
                     cancelled_at = NOW()
                 WHERE id = $1"
            )
            .bind(order_id)
            .bind(reason)
            .execute(&ctx.deps().db)
            .await?;
        }
    }

    Ok(())
}

// Query: Get order details
async fn get_order(order_id: Uuid, db: &PgPool) -> Result<Order> {
    sqlx::query_as(
        "SELECT * FROM orders_view WHERE id = $1"
    )
    .bind(order_id)
    .fetch_one(db)
    .await
}

// ============================================================================
// QUERY SIDE - Read Model 2: User Dashboard (Denormalized)
// ============================================================================

#[handler(on = OrderEvent)]
async fn update_user_dashboard(
    event: OrderEvent,
    ctx: Ctx,
) -> Result<()> {
    match event {
        OrderEvent::OrderPlaced { user_id, total, .. } => {
            sqlx::query(
                "INSERT INTO user_dashboard (user_id, total_orders, total_spent)
                 VALUES ($1, 1, $2)
                 ON CONFLICT (user_id) DO UPDATE
                 SET total_orders = user_dashboard.total_orders + 1,
                     total_spent = user_dashboard.total_spent + $2"
            )
            .bind(user_id)
            .bind(total)
            .execute(&ctx.deps().db)
            .await?;
        }
        OrderEvent::OrderCancelled { order_id, .. } => {
            // Decrement counters
            let order = sqlx::query_as::<_, Order>(
                "SELECT * FROM orders_view WHERE id = $1"
            )
            .bind(order_id)
            .fetch_one(&ctx.deps().db)
            .await?;

            sqlx::query(
                "UPDATE user_dashboard
                 SET total_orders = total_orders - 1,
                     total_spent = total_spent - $2
                 WHERE user_id = $1"
            )
            .bind(order.user_id)
            .bind(order.total)
            .execute(&ctx.deps().db)
            .await?;
        }
        _ => {}
    }

    Ok(())
}

// Query: Get user dashboard
async fn get_user_dashboard(user_id: Uuid, db: &PgPool) -> Result<UserDashboard> {
    sqlx::query_as(
        "SELECT * FROM user_dashboard WHERE user_id = $1"
    )
    .bind(user_id)
    .fetch_one(db)
    .await
}

// ============================================================================
// QUERY SIDE - Read Model 3: Analytics (Aggregates)
// ============================================================================

#[handler(on = OrderEvent)]
async fn update_analytics(
    event: OrderEvent,
    ctx: Ctx,
) -> Result<()> {
    match event {
        OrderEvent::OrderPlaced { total, .. } => {
            sqlx::query(
                "INSERT INTO daily_sales (date, order_count, revenue)
                 VALUES (CURRENT_DATE, 1, $1)
                 ON CONFLICT (date) DO UPDATE
                 SET order_count = daily_sales.order_count + 1,
                     revenue = daily_sales.revenue + $1"
            )
            .bind(total)
            .execute(&ctx.deps().db)
            .await?;
        }
        _ => {}
    }

    Ok(())
}

// Query: Get daily sales
async fn get_daily_sales(db: &PgPool) -> Result<Vec<DailySales>> {
    sqlx::query_as(
        "SELECT * FROM daily_sales ORDER BY date DESC LIMIT 30"
    )
    .fetch_all(db)
    .await
}

// ============================================================================
// QUERY SIDE - Read Model 4: Search (Elasticsearch)
// ============================================================================

#[handler(on = OrderEvent)]
async fn update_search_index(
    event: OrderEvent,
    ctx: Ctx,
) -> Result<()> {
    match event {
        OrderEvent::OrderPlaced { order_id, user_id, items, total } => {
            ctx.deps().elasticsearch
                .index(IndexParts::IndexId("orders", &order_id.to_string()))
                .body(json!({
                    "order_id": order_id,
                    "user_id": user_id,
                    "items": items,
                    "total": total,
                    "status": "placed",
                    "created_at": Utc::now(),
                }))
                .send()
                .await?;
        }
        OrderEvent::OrderShipped { order_id, .. } => {
            ctx.deps().elasticsearch
                .update(UpdateParts::IndexId("orders", &order_id.to_string()))
                .body(json!({ "doc": { "status": "shipped" } }))
                .send()
                .await?;
        }
        // ... other events
        _ => {}
    }

    Ok(())
}

// Query: Search orders
async fn search_orders(
    query: &str,
    es: &Elasticsearch,
) -> Result<Vec<Order>> {
    let response = es
        .search(SearchParts::Index(&["orders"]))
        .body(json!({
            "query": {
                "multi_match": {
                    "query": query,
                    "fields": ["order_id", "user_id", "items"]
                }
            }
        }))
        .send()
        .await?;

    // Parse response...
    Ok(orders)
}

// ============================================================================
// API ENDPOINTS
// ============================================================================

// Command endpoints
#[post("/orders")]
async fn create_order_api(
    cmd: Json<CreateOrder>,
    engine: Data<Engine>,
    db: Data<PgPool>,
) -> Result<Json<Uuid>> {
    let order_id = handle_create_order(cmd.into_inner(), &engine, &db).await?;
    Ok(Json(order_id))
}

#[post("/orders/{id}/cancel")]
async fn cancel_order_api(
    order_id: Path<Uuid>,
    cmd: Json<CancelOrder>,
    engine: Data<Engine>,
    db: Data<PgPool>,
) -> Result<HttpResponse> {
    handle_cancel_order(cmd.into_inner(), &engine, &db).await?;
    Ok(HttpResponse::Ok().finish())
}

// Query endpoints
#[get("/orders/{id}")]
async fn get_order_api(
    order_id: Path<Uuid>,
    db: Data<PgPool>,
) -> Result<Json<Order>> {
    let order = get_order(*order_id, &db).await?;
    Ok(Json(order))
}

#[get("/users/{id}/dashboard")]
async fn get_dashboard_api(
    user_id: Path<Uuid>,
    db: Data<PgPool>,
) -> Result<Json<UserDashboard>> {
    let dashboard = get_user_dashboard(*user_id, &db).await?;
    Ok(Json(dashboard))
}

#[get("/orders/search")]
async fn search_orders_api(
    query: Query<SearchQuery>,
    es: Data<Elasticsearch>,
) -> Result<Json<Vec<Order>>> {
    let orders = search_orders(&query.q, &es).await?;
    Ok(Json(orders))
}
```

---

## Part 7: CQRS Anti-Patterns to Avoid

### Anti-Pattern 1: Querying Write Model

```rust
// ❌ BAD: Query reads from write model
#[get("/orders/{id}")]
async fn get_order(order_id: Path<Uuid>, db: Data<PgPool>) -> Result<Json<Order>> {
    // Reading from event store (write model)
    let events = sqlx::query("SELECT * FROM seesaw_events WHERE ...").fetch_all(&db).await?;
    let order = reconstruct_from_events(events)?;
    Ok(Json(order))
}

// ✅ GOOD: Query reads from read model
#[get("/orders/{id}")]
async fn get_order(order_id: Path<Uuid>, db: Data<PgPool>) -> Result<Json<Order>> {
    // Reading from orders_view (read model)
    let order = sqlx::query_as("SELECT * FROM orders_view WHERE id = $1")
        .bind(*order_id)
        .fetch_one(&db)
        .await?;
    Ok(Json(order))
}
```

### Anti-Pattern 2: Commands Returning Query Data

```rust
// ❌ BAD: Command returns read model data
async fn create_order(cmd: CreateOrder, engine: &Engine, db: &PgPool) -> Result<OrderDetails> {
    let order_id = Uuid::new_v4();
    engine.dispatch(OrderPlaced { order_id, ... }).await?;

    // Returning full order details (read model)
    let order = get_order(order_id, db).await?;
    Ok(order)  // ❌ Mixing command and query!
}

// ✅ GOOD: Command returns only ID
async fn create_order(cmd: CreateOrder, engine: &Engine) -> Result<Uuid> {
    let order_id = Uuid::new_v4();
    engine.dispatch(OrderPlaced { order_id, ... }).await?;
    Ok(order_id)  // ✅ Just the ID, client queries separately
}

// Client does two calls:
let order_id = create_order(cmd).await?;
let order = get_order(order_id).await?;  // Separate query
```

### Anti-Pattern 3: Read Models in Command Validation

```rust
// ⚠️ CAREFUL: Using read model for validation
async fn cancel_order(cmd: CancelOrder, engine: &Engine, db: &PgPool) -> Result<()> {
    // Reading from read model for validation
    let order = sqlx::query_as::<_, Order>(
        "SELECT * FROM orders_view WHERE id = $1"
    )
    .bind(cmd.order_id)
    .fetch_one(db)
    .await?;

    // Validation based on read model
    if order.status != "placed" {
        bail!("Can only cancel placed orders");
    }

    engine.dispatch(OrderCancelled { ... }).await?;
    Ok(())
}
```

**This is OK for:**
- Performance (read models are optimized for reading)
- Non-critical validation

**Use event sourcing instead for:**
- Critical business rules (load from events for source of truth)
- Complex state transitions (reconstruct aggregate)
- Audit compliance (verify against event store)

---

## Conclusion

**Yes, Seesaw is highly compatible with CQRS.**

**Seesaw's role:**
- ✅ Event store (source of truth)
- ✅ Event dispatcher (pub/sub)
- ✅ Projection builder (handlers update read models)
- ✅ Retry + reliability (at-least-once delivery)

**You add:**
- Command validation (thin layer)
- Command routing (API endpoints)
- Query execution (read from read models)
- Aggregate logic (optional, for complex domains)

**The new async-first architecture makes CQRS even better:**
- ✅ Writes don't block on read model updates (10ms vs 225ms)
- ✅ Read models update independently (natural parallelism)
- ✅ Slow read models don't affect fast ones (separate streams)
- ✅ Scales to high throughput (1k-100k events/sec)

**Seesaw is the perfect backbone for CQRS systems.**
