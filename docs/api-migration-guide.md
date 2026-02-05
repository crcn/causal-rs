# Seesaw API Migration Guide: v0.7 → Queue-Backed

Complete example showing API changes for a multi-day order processing workflow.

## Example: Order Processing System

**Business flow:**
1. Order placed → charge payment immediately
2. Send confirmation email immediately
3. Wait 2 days → request approval from manager
4. Wait 7 days → send reminder if no approval
5. On approval → finalize order

---

## 1. Event Definitions

### v0.7 (Stateless) ❌

```rust
use serde::{Serialize, Deserialize};
use uuid::Uuid;

// Events carry correlation_id (boilerplate!)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    correlation_id: Uuid,           // ← Manual saga tracking
    customer_email: String,
    amount_cents: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaymentCharged {
    order_id: Uuid,
    correlation_id: Uuid,           // ← Must pass through
    charge_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfirmationSent {
    order_id: Uuid,
    correlation_id: Uuid,           // ← Repeated in every event
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalRequested {
    order_id: Uuid,
    correlation_id: Uuid,
    requested_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReminderSent {
    order_id: Uuid,
    correlation_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalReceived {
    order_id: Uuid,
    correlation_id: Uuid,
    approver_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderCompleted {
    order_id: Uuid,
    correlation_id: Uuid,
}
```

### Queue-Backed ✅

```rust
use serde::{Serialize, Deserialize};
use uuid::Uuid;

// Events are pure business data (no correlation_id!)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    customer_email: String,
    amount_cents: i64,
    // correlation_id removed! Framework manages it ✅
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaymentCharged {
    order_id: Uuid,
    charge_id: String,
    // No correlation_id ✅
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfirmationSent {
    order_id: Uuid,
    // No correlation_id ✅
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalRequested {
    order_id: Uuid,
    requested_at: DateTime<Utc>,
    // No correlation_id ✅
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReminderSent {
    order_id: Uuid,
    // No correlation_id ✅
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalReceived {
    order_id: Uuid,
    approver_id: Uuid,
    // No correlation_id ✅
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderCompleted {
    order_id: Uuid,
    // No correlation_id ✅
}
```

---

## 2. State Definition

### v0.7 (Stateless)

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OrderState {
    order_id: Option<Uuid>,
    status: OrderStatus,
    payment_charged: bool,
    confirmation_sent: bool,
    approval_requested: bool,
    reminder_sent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderStatus {
    Pending,
    AwaitingApproval,
    Approved,
    Completed,
}

impl Default for OrderStatus {
    fn default() -> Self {
        Self::Pending
    }
}
```

### Queue-Backed ✅

```rust
// Same state definition - no changes needed
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OrderState {
    order_id: Option<Uuid>,
    status: OrderStatus,
    payment_charged: bool,
    confirmation_sent: bool,
    approval_requested: bool,
    reminder_sent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderStatus {
    Pending,
    AwaitingApproval,
    Approved,
    Completed,
}

impl Default for OrderStatus {
    fn default() -> Self {
        Self::Pending
    }
}
```

---

## 3. Dependencies

### v0.7 (Stateless)

```rust
#[derive(Clone)]
struct Deps {
    stripe: Arc<StripeClient>,
    mailer: Arc<MailerClient>,
    db: PgPool,
}
```

### Queue-Backed ✅

```rust
// Same - no changes needed
#[derive(Clone)]
struct Deps {
    stripe: Arc<StripeClient>,
    mailer: Arc<MailerClient>,
    db: PgPool,
}
```

---

## 4. Effects

### v0.7 (Stateless) ❌

```rust
use seesaw::{effect, Engine};

// Effect 1: Charge payment (no timeout/retry config)
let charge_effect = effect::on::<OrderPlaced>()
    .then(|event, ctx| async move {
        // Manual idempotency check (easy to forget!)
        if ctx.deps().db.already_charged(event.order_id).await? {
            return Ok(());
        }

        let charge = ctx.deps().stripe.charge(
            event.amount_cents,
            // Manual idempotency key generation
            idempotency_key: &format!("order-{}", event.order_id)
        ).await?;

        // Manual idempotency mark
        ctx.deps().db.mark_charged(event.order_id).await?;

        // Have to pass correlation_id through
        ctx.emit(PaymentCharged {
            order_id: event.order_id,
            correlation_id: event.correlation_id,  // ← Manual pass-through
            charge_id: charge.id,
        })?;

        Ok(())
    });

// Effect 2: Send confirmation
let confirm_effect = effect::on::<OrderPlaced>()
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_confirmation(&event.customer_email).await?;

        ctx.emit(ConfirmationSent {
            order_id: event.order_id,
            correlation_id: event.correlation_id,  // ← Manual pass-through
        })?;

        Ok(())
    });

// Effect 3: Request approval (no way to delay!)
let approval_effect = effect::on::<OrderPlaced>()
    .then(|event, ctx| async move {
        // Can't delay 2 days - would need manual scheduling
        ctx.deps().mailer.send_approval_request(&event).await?;

        ctx.emit(ApprovalRequested {
            order_id: event.order_id,
            correlation_id: event.correlation_id,
            requested_at: Utc::now(),
        })?;

        Ok(())
    });

// Effect 4: Send reminder (no way to delay!)
let reminder_effect = effect::on::<ApprovalRequested>()
    .then(|event, ctx| async move {
        // Can't delay 7 days automatically
        ctx.deps().mailer.send_reminder(event.order_id).await?;

        ctx.emit(ReminderSent {
            order_id: event.order_id,
            correlation_id: event.correlation_id,
        })?;

        Ok(())
    });

// Effect 5: Complete order
let complete_effect = effect::on::<ApprovalReceived>()
    .then(|event, ctx| async move {
        ctx.deps().db.finalize_order(event.order_id).await?;

        ctx.emit(OrderCompleted {
            order_id: event.order_id,
            correlation_id: event.correlation_id,
        })?;

        Ok(())
    });
```

### Queue-Backed ✅

```rust
use seesaw::{effect, Engine};

// Effect 1: Charge payment (with timeout, retry, priority)
let charge_effect = effect::on::<OrderPlaced>()
    .id("charge_payment")                    // ← Required (compile-time enforced)
    .timeout(Duration::from_secs(60))        // ← Stripe can be slow
    .retry(5)                                // ← Critical - retry more
    .priority(1)                             // ← High priority
    .then(|event, ctx| async move {
        // No manual idempotency - framework guarantees it!
        let charge = ctx.deps().stripe.charge(
            event.amount_cents,
            idempotency_key: &ctx.idempotency_key  // ← Framework provides UUID v5
        ).await?;

        // correlation_id automatically attached by framework!
        Ok(PaymentCharged {
            order_id: event.order_id,
            charge_id: charge.id,
            // No correlation_id needed! ✅
        })
    });

// Effect 2: Send confirmation (immediate)
let confirm_effect = effect::on::<OrderPlaced>()
    .id("send_confirmation")
    .timeout(Duration::from_secs(10))
    .retry(3)
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_confirmation(&event.customer_email).await?;

        Ok(ConfirmationSent {
            order_id: event.order_id,
        })
    });

// Effect 3: Request approval (delayed 2 days!)
let approval_effect = effect::on::<OrderPlaced>()
    .id("request_approval")
    .delayed(Duration::from_days(2))         // ← Execute in 2 days!
    .timeout(Duration::from_secs(30))
    .retry(5)
    .then(|event, ctx| async move {
        ctx.deps().mailer.send_approval_request(&event).await?;

        Ok(ApprovalRequested {
            order_id: event.order_id,
            requested_at: Utc::now(),
        })
    });

// Effect 4: Send reminder (delayed 7 days from approval request!)
let reminder_effect = effect::on::<ApprovalRequested>()
    .id("send_reminder")
    .delayed(Duration::from_days(7))         // ← Execute in 7 days!
    .retry(3)
    .then(|event, ctx| async move {
        // Only send if not approved yet
        if ctx.next_state().status == OrderStatus::AwaitingApproval {
            ctx.deps().mailer.send_reminder(event.order_id).await?;
            Ok(ReminderSent { order_id: event.order_id })
        } else {
            Ok(())  // Already approved, skip reminder
        }
    });

// Effect 5: Complete order
let complete_effect = effect::on::<ApprovalReceived>()
    .id("complete_order")
    .timeout(Duration::from_secs(30))
    .then(|event, ctx| async move {
        ctx.deps().db.finalize_order(event.order_id).await?;

        Ok(OrderCompleted {
            order_id: event.order_id,
        })
    });
```

---

## 5. Effects with `on!` Macro

### v0.7 (Stateless)

```rust
// No on! macro in v0.7
```

### Queue-Backed ✅

```rust
use seesaw::on;

let effects = on! {
    // Critical payment - high priority, long timeout
    #[effect(id = "charge_payment", priority = 1, timeout = 60, retry = 5)]
    OrderPlaced { order_id, amount_cents, .. } => |ctx| async move {
        let charge = ctx.deps().stripe.charge(
            amount_cents,
            idempotency_key: &ctx.idempotency_key
        ).await?;
        Ok(PaymentCharged { order_id, charge_id: charge.id })
    },

    // Immediate confirmation
    #[effect(id = "send_confirmation", timeout = 10, retry = 3)]
    OrderPlaced { order_id, customer_email, .. } => |ctx| async move {
        ctx.deps().mailer.send_confirmation(&customer_email).await?;
        Ok(ConfirmationSent { order_id })
    },

    // Delayed approval request (2 days)
    #[effect(id = "request_approval", delayed = 2days, retry = 5, timeout = 30)]
    OrderPlaced { order_id, .. } => |ctx| async move {
        ctx.deps().mailer.send_approval_request(order_id).await?;
        Ok(ApprovalRequested { order_id, requested_at: Utc::now() })
    },

    // Delayed reminder (7 days)
    #[effect(id = "send_reminder", delayed = 7days, retry = 3)]
    ApprovalRequested { order_id, .. } => |ctx| async move {
        if ctx.next_state().status == OrderStatus::AwaitingApproval {
            ctx.deps().mailer.send_reminder(order_id).await?;
            Ok(ReminderSent { order_id })
        } else {
            Ok(())
        }
    },

    // Complete order
    #[effect(id = "complete_order", timeout = 30)]
    ApprovalReceived { order_id, .. } => |ctx| async move {
        ctx.deps().db.finalize_order(order_id).await?;
        Ok(OrderCompleted { order_id })
    },
};
```

---

## 6. Reducers

### v0.7 (Stateless)

```rust
use seesaw::reducer;

let order_placed_reducer = reducer::on::<OrderPlaced>().run(|state, event| {
    OrderState {
        order_id: Some(event.order_id),
        status: OrderStatus::Pending,
        payment_charged: false,
        confirmation_sent: false,
        approval_requested: false,
        reminder_sent: false,
    }
});

let payment_charged_reducer = reducer::on::<PaymentCharged>().run(|state, _event| {
    OrderState {
        payment_charged: true,
        ..state
    }
});

let confirmation_sent_reducer = reducer::on::<ConfirmationSent>().run(|state, _event| {
    OrderState {
        confirmation_sent: true,
        ..state
    }
});

let approval_requested_reducer = reducer::on::<ApprovalRequested>().run(|state, _event| {
    OrderState {
        status: OrderStatus::AwaitingApproval,
        approval_requested: true,
        ..state
    }
});

let reminder_sent_reducer = reducer::on::<ReminderSent>().run(|state, _event| {
    OrderState {
        reminder_sent: true,
        ..state
    }
});

let approval_received_reducer = reducer::on::<ApprovalReceived>().run(|state, _event| {
    OrderState {
        status: OrderStatus::Approved,
        ..state
    }
});

let order_completed_reducer = reducer::on::<OrderCompleted>().run(|state, _event| {
    OrderState {
        status: OrderStatus::Completed,
        ..state
    }
});
```

### Queue-Backed ✅

```rust
use seesaw::reducer;

// Same reducer definitions - no changes needed!
let order_placed_reducer = reducer::on::<OrderPlaced>().run(|state, event| {
    OrderState {
        order_id: Some(event.order_id),
        status: OrderStatus::Pending,
        payment_charged: false,
        confirmation_sent: false,
        approval_requested: false,
        reminder_sent: false,
    }
});

let payment_charged_reducer = reducer::on::<PaymentCharged>().run(|state, _event| {
    OrderState {
        payment_charged: true,
        ..state
    }
});

let confirmation_sent_reducer = reducer::on::<ConfirmationSent>().run(|state, _event| {
    OrderState {
        confirmation_sent: true,
        ..state
    }
});

let approval_requested_reducer = reducer::on::<ApprovalRequested>().run(|state, _event| {
    OrderState {
        status: OrderStatus::AwaitingApproval,
        approval_requested: true,
        ..state
    }
});

let reminder_sent_reducer = reducer::on::<ReminderSent>().run(|state, _event| {
    OrderState {
        reminder_sent: true,
        ..state
    }
});

let approval_received_reducer = reducer::on::<ApprovalReceived>().run(|state, _event| {
    OrderState {
        status: OrderStatus::Approved,
        ..state
    }
});

let order_completed_reducer = reducer::on::<OrderCompleted>().run(|state, _event| {
    OrderState {
        status: OrderStatus::Completed,
        ..state
    }
});
```

---

## 7. Engine Setup & Execution

### v0.7 (Stateless) ❌

```rust
use seesaw::Engine;

#[tokio::main]
async fn main() -> Result<()> {
    let deps = Deps {
        stripe: Arc::new(StripeClient::new()),
        mailer: Arc::new(MailerClient::new()),
        db: PgPool::connect(&env::var("DATABASE_URL")?).await?,
    };

    // Build engine
    let engine = Engine::with_deps(deps)
        .with_effect(charge_effect)
        .with_effect(confirm_effect)
        .with_effect(approval_effect)
        .with_effect(reminder_effect)
        .with_effect(complete_effect)
        .with_reducer(order_placed_reducer)
        .with_reducer(payment_charged_reducer)
        .with_reducer(confirmation_sent_reducer)
        .with_reducer(approval_requested_reducer)
        .with_reducer(reminder_sent_reducer)
        .with_reducer(approval_received_reducer)
        .with_reducer(order_completed_reducer);

    // Stateless execution - no durability!
    let handle = engine.activate(OrderState::default());

    // Process initial event
    handle.run(|_ctx| {
        Ok(OrderPlaced {
            order_id: Uuid::new_v4(),
            correlation_id: Uuid::new_v4(),  // ← Manual correlation_id
            customer_email: "user@example.com".to_string(),
            amount_cents: 9999,
        })
    }).await?;

    // Wait for all effects (in-memory only, not durable!)
    handle.settled().await?;

    // ❌ Problem: No way to handle delayed effects (2 days, 7 days)
    // ❌ Problem: No way to handle approval webhook that comes later
    // ❌ Problem: Everything must complete in one run

    Ok(())
}

// Webhook handler (Day 3)
async fn handle_approval_webhook(order_id: Uuid) -> Result<()> {
    // ❌ How do we continue the saga from 2 days ago?
    // ❌ No correlation_id lookup mechanism
    // ❌ State is lost after handle.settled() completed

    panic!("Can't handle multi-day workflows in stateless mode!")
}
```

### Queue-Backed ✅

```rust
use seesaw::{Engine, PostgresQueue};
use sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<()> {
    let pool = PgPool::connect(&env::var("DATABASE_URL")?).await?;

    let deps = Deps {
        stripe: Arc::new(StripeClient::new()),
        mailer: Arc::new(MailerClient::new()),
        db: pool.clone(),
    };

    // Build engine (same as v0.7)
    let engine = Engine::new(deps)
        .with_effect(charge_effect)
        .with_effect(confirm_effect)
        .with_effect(approval_effect)
        .with_effect(reminder_effect)
        .with_effect(complete_effect)
        .with_reducer(order_placed_reducer)
        .with_reducer(payment_charged_reducer)
        .with_reducer(confirmation_sent_reducer)
        .with_reducer(approval_requested_reducer)
        .with_reducer(reminder_sent_reducer)
        .with_reducer(approval_received_reducer)
        .with_reducer(order_completed_reducer);

    // Create queue and start two-phase workers
    let queue = PostgresQueue::new(pool)
        .start_workers(
            2,   // Event workers (fast - state updates)
            20,  // Effect workers (slow - IO)
            engine.clone()
        )
        .await?;

    let engine = engine.with_queue(queue);

    // Process initial event (starts new saga)
    let correlation_id = engine.process(|| async {
        Ok(OrderPlaced {
            order_id: Uuid::new_v4(),
            customer_email: "user@example.com".to_string(),
            amount_cents: 9999,
            // No correlation_id! Framework generates it ✅
        })
    }).await?;

    info!("Started order saga: {}", correlation_id);

    // Store correlation_id for later (webhook callbacks)
    sqlx::query!(
        "UPDATE orders SET correlation_id = $1 WHERE order_id = $2",
        correlation_id,
        order_id
    )
    .execute(&pool)
    .await?;

    // ✅ Workers handle everything:
    //    - Immediate: charge payment, send confirmation
    //    - Day 2: send approval request
    //    - Day 9: send reminder (if still pending)

    // Keep running
    tokio::signal::ctrl_c().await?;
    engine.shutdown().await?;  // Graceful drain

    Ok(())
}

// Webhook handler (Day 3) - Continue existing saga!
async fn handle_approval_webhook(
    order_id: Uuid,
    approver_id: Uuid,
    engine: Arc<Engine<OrderState, Deps>>,
    pool: &PgPool,
) -> Result<()> {
    // ✅ Lookup correlation_id from database
    let correlation_id = sqlx::query!(
        "SELECT correlation_id FROM orders WHERE order_id = $1",
        order_id
    )
    .fetch_one(pool)
    .await?
    .correlation_id;

    // ✅ Continue existing saga (state preserved from Day 1!)
    engine.process_saga(correlation_id, || async move {
        Ok(ApprovalReceived {
            order_id,
            approver_id,
            // No correlation_id! ✅
        })
    }).await?;

    Ok(())
}
```

---

## 8. Testing

### v0.7 (Stateless)

```rust
#[tokio::test]
async fn test_order_flow() {
    let deps = create_test_deps();
    let engine = create_test_engine(deps);
    let handle = engine.activate(OrderState::default());

    handle.run(|_| Ok(OrderPlaced { /* ... */ })).await.unwrap();
    handle.settled().await.unwrap();

    let state = handle.state();
    assert_eq!(state.status, OrderStatus::Pending);
    assert!(state.payment_charged);
    assert!(state.confirmation_sent);

    // ❌ Can't test delayed effects (2 days, 7 days)
    // ❌ Can't test multi-day workflows
}
```

### Queue-Backed ✅

```rust
#[tokio::test]
async fn test_order_flow() {
    let pool = create_test_pool().await;
    let deps = create_test_deps();
    let engine = create_test_engine(deps);

    let queue = PostgresQueue::new(pool.clone())
        .start_workers(1, 5, engine.clone())
        .await
        .unwrap();

    let engine = engine.with_queue(queue);

    // Day 1: Place order
    let correlation_id = engine.process(|| async {
        Ok(OrderPlaced {
            order_id: Uuid::new_v4(),
            customer_email: "test@example.com".to_string(),
            amount_cents: 9999,
        })
    }).await.unwrap();

    // Wait for immediate effects
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Check state
    let state = get_saga_state(&pool, correlation_id).await.unwrap();
    assert_eq!(state.status, OrderStatus::Pending);
    assert!(state.payment_charged);
    assert!(state.confirmation_sent);

    // ✅ Test delayed effects by advancing time
    // Set execute_at to NOW for testing
    sqlx::query!(
        "UPDATE seesaw_effect_executions
         SET execute_at = NOW()
         WHERE event_id IN (
             SELECT event_id FROM seesaw_events WHERE correlation_id = $1
         )",
        correlation_id
    )
    .execute(&pool)
    .await
    .unwrap();

    // Wait for delayed effects to execute
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Check approval requested
    let state = get_saga_state(&pool, correlation_id).await.unwrap();
    assert_eq!(state.status, OrderStatus::AwaitingApproval);

    engine.shutdown().await.unwrap();
}
```

---

## Summary of Changes

| Aspect | v0.7 Stateless | Queue-Backed | Benefit |
|--------|---------------|--------------|---------|
| **Event correlation_id** | Manual in every event | Envelope carries it | -50 LOC, cleaner events |
| **Effect ID** | Optional | **Required** (`.id()`) | Compile-time idempotency |
| **Idempotency** | Manual (15 lines per effect) | Framework-guaranteed | -300 LOC, no footguns |
| **Delayed effects** | ❌ Not possible | `.delayed(2days)` | Multi-day workflows |
| **Timeout config** | ❌ Global only | Per-effect (`.timeout(60)`) | Payment: 60s, Email: 10s |
| **Retry config** | ❌ Global only | Per-effect (`.retry(5)`) | Critical: 5x, Logs: 1x |
| **Priority** | ❌ No priority | `.priority(1)` | Critical first |
| **Workers** | ❌ No workers | 2 event + 20 effect | Scalability |
| **Multi-day** | ❌ Not possible | ✅ State persists | Days 1-9 workflow works |
| **Graceful shutdown** | ❌ No shutdown | `engine.shutdown()` | No data loss on deploy |
| **Execution** | `activate()` + `run()` | `process()` + workers | Durable, scalable |
| **LOC** | ~500 LOC (with boilerplate) | ~200 LOC (clean) | -60% code |

---

## Migration Checklist

- [ ] Remove `correlation_id` from all event structs
- [ ] Add `.id("name")` to all effects (compile enforces)
- [ ] Remove manual idempotency checks (framework handles it)
- [ ] Add `.delayed()` for time-based effects
- [ ] Add `.timeout()` and `.retry()` per effect
- [ ] Change `engine.activate()` → `queue.start_workers()`
- [ ] Change `handle.run()` → `engine.process()`
- [ ] Store correlation_id for webhook continuations
- [ ] Use `engine.process_saga()` for continuing sagas
- [ ] Add graceful shutdown handler
- [ ] Run database migrations (`001_queue_tables.sql`)
- [ ] Update connection pool: `max_connections = workers * 2`

---

## Key Takeaways

1. **Events are cleaner** - No correlation_id boilerplate
2. **Idempotency is guaranteed** - Compile-time enforced via `.id()`
3. **Delays are built-in** - `.delayed(7days)` just works
4. **Per-effect config** - Payment: 60s timeout, Email: 10s
5. **Multi-day workflows** - State persists between days 1-9
6. **Two-phase workers** - Fast state updates, slow effects scale independently
7. **Production-ready** - Graceful shutdown, DLQ, infinite loop protection
