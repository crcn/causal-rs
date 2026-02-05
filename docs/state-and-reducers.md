# State & Reducers in Queue-Backed Seesaw

## TL;DR

- **State is per-saga** - Each workflow has isolated state
- **Reducers are pure** - Transform state before effects run
- **Effects see latest** - Load fresh state from DB (not snapshots)
- **Two-phase execution** - Reducers in Phase 1, Effects in Phase 2

---

## 1. State Model

### Per-Saga Isolation

Each saga (workflow) has its own state stored in the database:

```sql
CREATE TABLE seesaw_state (
    saga_id UUID PRIMARY KEY,
    state JSONB NOT NULL,
    version INT NOT NULL DEFAULT 1,  -- For optimistic locking
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

**Example**: E-commerce order workflow

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderState {
    order_id: String,
    user_id: String,
    status: OrderStatus,
    total: f64,
    items: Vec<Item>,
    payment_confirmed: bool,
    shipped_at: Option<DateTime<Utc>>,
}

impl Default for OrderState {
    fn default() -> Self {
        Self {
            order_id: String::new(),
            user_id: String::new(),
            status: OrderStatus::Pending,
            total: 0.0,
            items: vec![],
            payment_confirmed: false,
            shipped_at: None,
        }
    }
}
```

### State Initialization

State is created when first event is processed:

```rust
// User action: place order
let saga_id = engine.process(OrderPlaced {
    order_id: "ORD-123",
    user_id: "user_42",
    items: vec![...],
    total: 99.99,
}).await?;

// Framework creates initial state:
// INSERT INTO seesaw_state (saga_id, state, version)
// VALUES ($saga_id, '{"order_id":"ORD-123",...}', 1)
```

---

## 2. Reducers - Pure State Transformations

### What Are Reducers?

Reducers are **pure functions** that transform state based on events. They:
- ✅ Take current state + event → return new state
- ✅ Run **synchronously** (no async, no IO)
- ✅ Execute in **Phase 1** (before effects)
- ❌ Cannot call external services
- ❌ Cannot emit events

### Reducer API

```rust
use seesaw::reducer;

let order_reducer = reducer::on::<OrderEvent>()
    .run(|state, event| {
        match event {
            OrderEvent::Placed { order_id, items, total, .. } => {
                OrderState {
                    order_id: order_id.clone(),
                    items: items.clone(),
                    total: *total,
                    status: OrderStatus::Pending,
                    ..state.clone()
                }
            }

            OrderEvent::PaymentConfirmed { .. } => {
                OrderState {
                    payment_confirmed: true,
                    status: OrderStatus::PaidWaitingShipment,
                    ..state.clone()
                }
            }

            OrderEvent::Shipped { shipped_at, .. } => {
                OrderState {
                    shipped_at: Some(*shipped_at),
                    status: OrderStatus::Shipped,
                    ..state.clone()
                }
            }

            _ => state.clone(),  // Ignore other events
        }
    });
```

### Multiple Reducers

You can register multiple reducers that chain together:

```rust
let engine = Engine::new(deps)
    // Order lifecycle reducer
    .with_reducer(
        reducer::on::<OrderEvent>()
            .run(|state, event| { /* update order fields */ })
    )
    // Analytics reducer
    .with_reducer(
        reducer::on::<OrderEvent>()
            .run(|state, event| match event {
                OrderEvent::Placed { total, .. } => OrderState {
                    total_lifetime_value: state.total_lifetime_value + total,
                    order_count: state.order_count + 1,
                    ..state
                },
                _ => state,
            })
    );
```

---

## 3. Two-Phase Execution Flow

### Phase 1: Event Worker (Fast - Pure State)

```
┌─────────────────────────────────────────┐
│   Event Worker (CPU-bound, fast)       │
├─────────────────────────────────────────┤
│ 1. Poll event from queue (FOR UPDATE)  │
│ 2. Load current state + version        │
│ 3. Run ALL reducers → new state        │
│ 4. BEGIN TRANSACTION                    │
│    - Save new state (version++)        │
│    - Insert effect_executions (pending)│
│    - Mark event processed              │
│    COMMIT                               │
└─────────────────────────────────────────┘
```

**Code Flow**:
```rust
async fn process_event_worker(queue: &Queue, engine: &Engine) {
    // 1. Poll next event
    let event = queue.poll_next().await?;

    // 2. Load current state
    let (mut state, version) = state_store.load(event.saga_id).await?;

    // 3. Run all reducers (pure, in-memory)
    for reducer in engine.reducers() {
        if reducer.matches(&event) {
            state = reducer.apply(&state, &event);  // ← Pure function
        }
    }

    // 4. Atomic transaction
    let mut tx = pool.begin().await?;

    // Save new state
    sqlx::query!(
        "UPDATE seesaw_state
         SET state = $1, version = $2, updated_at = NOW()
         WHERE saga_id = $3 AND version = $4",
        Json(&state),
        version + 1,
        event.saga_id,
        version
    )
    .execute(&mut *tx)
    .await?;

    // Insert effect execution intents
    for effect in engine.effects() {
        if effect.matches(&event) {
            sqlx::query!(
                "INSERT INTO seesaw_effect_executions
                 (event_id, effect_id, saga_id, status, priority, execute_at)
                 VALUES ($1, $2, $3, 'pending', $4, $5)",
                event.event_id,
                effect.id,
                event.saga_id,
                effect.priority,
                Utc::now() + effect.delay
            )
            .execute(&mut *tx)
            .await?;
        }
    }

    // Mark event processed
    sqlx::query!(
        "UPDATE seesaw_events SET processed_at = NOW()
         WHERE event_id = $1",
        event.event_id
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;  // ← Atomic: state + effect intents
}
```

### Phase 2: Effect Worker (Slow - IO-Bound)

```
┌─────────────────────────────────────────┐
│   Effect Worker (IO-bound, slow)       │
├─────────────────────────────────────────┤
│ 1. Poll effect from effect_executions  │
│ 2. Load LATEST state (fresh from DB)   │  ← Key difference!
│ 3. Run effect (async, can do IO)       │
│ 4. BEGIN TRANSACTION                    │
│    - Insert next events (deterministic)│
│    - Mark effect completed             │
│    COMMIT                               │
└─────────────────────────────────────────┘
```

**Code Flow**:
```rust
async fn process_effect_worker(queue: &Queue, engine: &Engine) {
    // 1. Poll next effect intent
    let pending = queue.poll_next_effect().await?;

    // 2. Load LATEST state (NOT snapshot!)
    let state = state_store.load(pending.saga_id).await?;  // ← Fresh load

    // 3. Build context
    let ctx = EffectContext {
        state,  // ← Latest state
        deps: engine.deps(),
        saga_id: pending.saga_id,
        event_id: pending.event_id,
        idempotency_key: format!("{}:{}", pending.event_id, pending.effect_id),
    };

    // 4. Execute effect (async, can do IO)
    let result = effect.handler.call(event, ctx).await;

    // 5. Atomic transaction: publish + mark complete
    let mut tx = pool.begin().await?;

    if let Ok(Some(next_event)) = result {
        // Deterministic event ID (idempotent on retry)
        let next_event_id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("{}:{}", pending.event_id, pending.effect_id).as_bytes()
        );

        // Insert next event (ON CONFLICT DO NOTHING = idempotent)
        sqlx::query!(
            "INSERT INTO seesaw_events (event_id, saga_id, event_type, payload)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (event_id) DO NOTHING",
            next_event_id,
            pending.saga_id,
            type_name_of_val(&next_event),
            Json(&next_event)
        )
        .execute(&mut *tx)
        .await?;
    }

    // Mark effect completed
    sqlx::query!(
        "UPDATE seesaw_effect_executions
         SET status = 'completed', completed_at = NOW()
         WHERE event_id = $1 AND effect_id = $2",
        pending.event_id,
        pending.effect_id
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;  // ← Atomic: next event + completion
}
```

---

## 4. State Semantics: Latest vs. Snapshot

### Critical Decision: Effects See Latest State

**Scenario**: User changes email between event creation and effect execution

```
Day 1, 10:00 AM - OrderPlaced event created
                  User email: "old@example.com"
                  State saved with old email
                  Effect intent created: "send_confirmation"

Day 1, 11:00 AM - User updates profile
                  New email: "new@example.com"
                  (Separate saga, updates user table)

Day 1, 12:00 PM - Effect worker runs "send_confirmation"
                  Loads LATEST state
                  Email is now: "new@example.com" ✅
```

**Code**:
```rust
effect::on::<OrderPlaced>()
    .id("send_confirmation")
    .then(|event, ctx| async move {
        let state = ctx.state();  // ← Loads fresh from DB

        // Email is "new@example.com", not "old@example.com"
        ctx.deps().mailer.send_confirmation(
            state.user_email,  // ← Latest value
            &event.order_id
        ).await?;

        Ok(())
    });
```

### Why Latest, Not Snapshot?

**Advantages**:
1. **Correct**: Users expect emails to go to their current address
2. **Scalable**: No need to store state snapshots per effect (15GB/month bloat)
3. **Simple**: One source of truth (seesaw_state table)

**Trade-offs**:
1. **Non-determinism**: State can change between event creation and effect execution
2. **Requires awareness**: Effects must handle state evolution

**Mitigation**:
- Use **event fields** for immutable data (order_id, amount)
- Use **state** for mutable data (user email, shipping address)

---

## 5. State Versioning (Optimistic Locking)

### The Hot Saga Problem

**Problem**: Multiple events for same saga processed concurrently

```
Worker 1: Load state (v1) → Reducer → Save state (v2) ✅
Worker 2: Load state (v1) → Reducer → Save state (v2) ❌ Conflict!
```

**Solution**: Optimistic locking with version check

```rust
// Event worker saves state
let rows = sqlx::query!(
    "UPDATE seesaw_state
     SET state = $1, version = version + 1
     WHERE saga_id = $2 AND version = $3",  // ← Version check
    Json(&new_state),
    saga_id,
    expected_version
)
.execute(&tx)
.await?
.rows_affected();

if rows == 0 {
    // Version conflict - requeue event
    queue.requeue(event.event_id).await?;
    return Ok(());
}
```

**Guarantees**:
- ✅ Per-saga FIFO processing (retry on conflict)
- ✅ No lost updates
- ✅ State transitions are serializable

---

## 6. Complete Example: Order Workflow

```rust
use seesaw::{Engine, effect, reducer};

// ============================================================
// State Definition
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderState {
    order_id: String,
    user_email: String,
    status: OrderStatus,
    total: f64,
    payment_confirmed: bool,
    shipped_at: Option<DateTime<Utc>>,
}

impl Default for OrderState {
    fn default() -> Self {
        Self {
            order_id: String::new(),
            user_email: String::new(),
            status: OrderStatus::Pending,
            total: 0.0,
            payment_confirmed: false,
            shipped_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderStatus {
    Pending,
    PaidWaitingShipment,
    Shipped,
    Delivered,
}

// ============================================================
// Events
// ============================================================

#[derive(Debug, Clone)]
enum OrderEvent {
    Placed { order_id: String, user_email: String, total: f64 },
    PaymentConfirmed { order_id: String },
    Shipped { order_id: String, shipped_at: DateTime<Utc> },
    Delivered { order_id: String },
}

// ============================================================
// Reducer (Phase 1 - Pure State Transformation)
// ============================================================

fn build_reducer() -> Reducer<OrderState> {
    reducer::on::<OrderEvent>()
        .run(|state, event| {
            match event {
                OrderEvent::Placed { order_id, user_email, total } => {
                    OrderState {
                        order_id: order_id.clone(),
                        user_email: user_email.clone(),
                        total: *total,
                        status: OrderStatus::Pending,
                        payment_confirmed: false,
                        shipped_at: None,
                    }
                }

                OrderEvent::PaymentConfirmed { .. } => {
                    OrderState {
                        payment_confirmed: true,
                        status: OrderStatus::PaidWaitingShipment,
                        ..state.clone()
                    }
                }

                OrderEvent::Shipped { shipped_at, .. } => {
                    OrderState {
                        shipped_at: Some(*shipped_at),
                        status: OrderStatus::Shipped,
                        ..state.clone()
                    }
                }

                OrderEvent::Delivered { .. } => {
                    OrderState {
                        status: OrderStatus::Delivered,
                        ..state.clone()
                    }
                }
            }
        })
}

// ============================================================
// Effects (Phase 2 - Side Effects with Latest State)
// ============================================================

fn build_effects() -> Vec<Effect<OrderState, Deps>> {
    vec![
        // Effect 1: Charge payment
        effect::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, total, .. } => {
                    Some((order_id.clone(), *total))
                },
                _ => None,
            })
            .id("charge_payment")
            .priority(5)  // High priority
            .timeout(Duration::from_secs(30))
            .retry(5)
            .then(|(order_id, total), ctx| async move {
                // Charge payment (uses idempotency key)
                ctx.deps().stripe.charge(
                    total,
                    &ctx.idempotency_key()  // ← Prevents double-charge
                ).await?;

                Ok(OrderEvent::PaymentConfirmed { order_id })
            }),

        // Effect 2: Send confirmation email
        effect::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::PaymentConfirmed { order_id } => {
                    Some(order_id.clone())
                },
                _ => None,
            })
            .id("send_confirmation")
            .then(|order_id, ctx| async move {
                let state = ctx.state();  // ← Load LATEST state

                // If user updated email, this gets the new one
                ctx.deps().mailer.send_confirmation(
                    &state.user_email,  // ← Latest email
                    &order_id
                ).await?;

                Ok(())  // No next event
            }),

        // Effect 3: Schedule shipment (delayed)
        effect::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::PaymentConfirmed { order_id } => {
                    Some(order_id.clone())
                },
                _ => None,
            })
            .id("schedule_shipment")
            .delayed(Duration::from_hours(24))  // Ship next day
            .then(|order_id, ctx| async move {
                let shipped_at = Utc::now();

                ctx.deps().fulfillment.ship(&order_id).await?;

                Ok(OrderEvent::Shipped { order_id, shipped_at })
            }),
    ]
}

// ============================================================
// Build Engine
// ============================================================

fn build_engine(deps: Deps) -> Engine<OrderState, Deps> {
    let mut engine = Engine::new(deps).with_reducer(build_reducer());

    for effect in build_effects() {
        engine = engine.with_effect(effect);
    }

    engine
}
```

---

## 7. Execution Timeline Example

```
T=0: User places order
     → engine.process(OrderEvent::Placed { order_id: "ORD-123", ... })

T=1: Event Worker (Phase 1)
     → Load state (empty/default)
     → Run reducer: state.status = Pending, state.total = 99.99
     → Save state (version 1)
     → Insert effect intents:
        - charge_payment (priority 5, execute_at: NOW)
        - send_confirmation (priority 10, execute_at: NOW)
        - schedule_shipment (priority 10, execute_at: NOW + 24h)

T=2: Effect Worker picks up "charge_payment" (highest priority)
     → Load LATEST state (version 1, status=Pending)
     → Call Stripe API (idempotency key prevents double-charge)
     → Insert next event: PaymentConfirmed
     → Mark effect completed

T=3: Event Worker processes PaymentConfirmed
     → Load state (version 1)
     → Run reducer: state.status = PaidWaitingShipment, payment_confirmed = true
     → Save state (version 2)
     → No new effect intents (already scheduled)

T=4: Effect Worker picks up "send_confirmation"
     → Load LATEST state (version 2, status=PaidWaitingShipment)
     → Send email to state.user_email
     → Mark effect completed

T=24h: Effect Worker picks up "schedule_shipment" (execute_at reached)
     → Load LATEST state (version 2 or higher)
     → Call fulfillment API
     → Insert next event: Shipped
     → Mark effect completed

T=24h+1: Event Worker processes Shipped
     → Load state (version 2)
     → Run reducer: state.status = Shipped, shipped_at = NOW
     → Save state (version 3)
```

---

## 8. Key Takeaways

| Concept | Summary |
|---------|---------|
| **State** | Per-saga, stored in DB, versioned for optimistic locking |
| **Reducers** | Pure functions, run in Phase 1, update state synchronously |
| **Effects** | Impure handlers, run in Phase 2, see **latest** state |
| **Phase 1** | Event Worker: reducers → save state → insert effect intents (atomic) |
| **Phase 2** | Effect Worker: load latest → run effect → publish next events (atomic) |
| **Latest Semantics** | Effects see current state, not snapshot (correct + scalable) |
| **Versioning** | Optimistic locking prevents lost updates on hot sagas |
| **Idempotency** | Deterministic event IDs + ON CONFLICT prevent duplicates |

---

## 9. Common Patterns

### Pattern 1: State Machine

```rust
reducer::on::<OrderEvent>()
    .run(|state, event| {
        let next_status = match (&state.status, event) {
            (OrderStatus::Pending, OrderEvent::PaymentConfirmed { .. }) => {
                OrderStatus::PaidWaitingShipment
            }
            (OrderStatus::PaidWaitingShipment, OrderEvent::Shipped { .. }) => {
                OrderStatus::Shipped
            }
            (OrderStatus::Shipped, OrderEvent::Delivered { .. }) => {
                OrderStatus::Delivered
            }
            _ => state.status.clone(),  // Invalid transition, ignore
        };

        OrderState {
            status: next_status,
            ..state.clone()
        }
    })
```

### Pattern 2: Accumulator

```rust
reducer::on::<AnalyticsEvent>()
    .run(|state, event| {
        AnalyticsState {
            total_revenue: state.total_revenue + event.amount,
            event_count: state.event_count + 1,
            last_event_at: Utc::now(),
            ..state
        }
    })
```

### Pattern 3: Conditional Effects Based on State

```rust
effect::on::<OrderPlaced>()
    .id("fraud_check")
    .then(|event, ctx| async move {
        let state = ctx.state();

        // Only check fraud if total > $500
        if state.total > 500.0 {
            let result = ctx.deps().fraud_checker.check(&event).await?;

            if result.is_suspicious {
                return Ok(OrderEvent::FlaggedForReview { order_id: event.order_id });
            }
        }

        Ok(OrderEvent::Approved { order_id: event.order_id })
    })
```

---

**State and reducers are now fully explained!** 🎯
