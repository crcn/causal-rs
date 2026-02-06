# Seesaw User Guide (Stateless Runtime)

Seesaw is a durable event/effect runtime.
Workflows are modeled as events; effects react to events and emit more events.

## Mental Model

- No reducers
- No workflow state object
- No `ctx.prev_state()` / `ctx.next_state()`
- Determinism comes from event payload + idempotent deps calls

## 1. Define Events (Event-Carried Context)

```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    OrderPlaced {
        order_id: Uuid,
        customer_email: String,
        amount_cents: i64,
    },
    PaymentCharged {
        order_id: Uuid,
        customer_email: String,
        amount_cents: i64,
    },
    ApprovalRequested {
        order_id: Uuid,
        customer_email: String,
    },
    ApprovalReceived {
        order_id: Uuid,
        customer_email: String,
    },
    OrderCompleted {
        order_id: Uuid,
    },
}
```

## 2. Define Dependencies

```rust
#[derive(Clone)]
struct Deps {
    stripe: StripeClient,
    mailer: MailerClient,
}
```

## 3. Register Effects

```rust
use seesaw::{effect, EffectContext, Engine};
use std::time::Duration;

fn build_engine<St: seesaw::Store>(deps: Deps, store: St) -> Engine<Deps, St> {
    Engine::new(deps, store)
        .with_handler(
            effect::on::<OrderEvent>()
                .extract(|event| match event {
                    OrderEvent::OrderPlaced {
                        order_id,
                        customer_email,
                        amount_cents,
                    } => Some((*order_id, customer_email.clone(), *amount_cents)),
                    _ => None,
                })
                .id("charge_payment")
                .retry(5)
                .timeout(Duration::from_secs(60))
                .then(|(order_id, customer_email, amount_cents), ctx: EffectContext<Deps>| async move {
                    ctx.deps()
                        .stripe
                        .charge(amount_cents, ctx.idempotency_key())
                        .await?;

                    Ok(OrderEvent::PaymentCharged {
                        order_id,
                        customer_email,
                        amount_cents,
                    })
                }),
        )
        .with_handler(
            effect::on::<OrderEvent>()
                .extract(|event| match event {
                    OrderEvent::PaymentCharged {
                        order_id,
                        customer_email,
                        ..
                    } => Some((*order_id, customer_email.clone())),
                    _ => None,
                })
                .id("request_approval")
                .delayed(Duration::from_secs(60 * 60 * 24 * 2))
                .then(|(order_id, customer_email), _ctx: EffectContext<Deps>| async move {
                    Ok(OrderEvent::ApprovalRequested {
                        order_id,
                        customer_email,
                    })
                }),
        )
}
```

## 4. Start Runtime

```rust
use seesaw::{Runtime, RuntimeConfig};

let engine = build_engine(deps, store);
let runtime = Runtime::start(
    &engine,
    RuntimeConfig {
        event_workers: 2,
        effect_workers: 8,
        ..Default::default()
    },
)
.await?;
```

`Runtime::start(...)` is fail-fast: started-hook errors are returned immediately.

## 5. Process Events

```rust
use uuid::Uuid;

// New workflow
engine
    .process(OrderEvent::OrderPlaced {
        order_id: Uuid::new_v4(),
        customer_email: "user@example.com".to_string(),
        amount_cents: 9999,
    })
    .await?;

// Continue existing workflow
let correlation_id = Uuid::parse_str("...")?;
engine
    .process_workflow(
        correlation_id,
        OrderEvent::ApprovalReceived {
            order_id: Uuid::parse_str("...")?,
            customer_email: "user@example.com".to_string(),
        },
    )
    .await?;
```

## 6. Shutdown

```rust
runtime.shutdown().await?;
```

## Migration Notes

- Replace reducer logic with event-carried fields.
- Remove all state access from effects.
- Prefer explicit `.id("...")` for queued/retry/delayed effects.
- If an effect needs current external data, query via `ctx.deps()`.
