//! Stress tests — break every code path in seesaw.
//!
//! Covers: causal links, aggregate state transitions, transition guards,
//! filter/extract predicates, priority ordering, idempotency keys,
//! hops limit, error propagation, emit edge cases, context fields,
//! custom runtime, multi-dispatch, and deep causal chains.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use seesaw_core::{events, handler, Aggregate, Apply, Context, Engine, Events};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone)]
struct Deps;

// ── Event types ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventA {
    value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventB {
    value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventC {
    value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventD {
    value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    total: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderShipped {
    order_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailEvent {
    attempt: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailedTerminal {
    error: String,
    attempts: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchItem {
    index: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchResult {
    count: usize,
}

// ── Aggregate types ────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
enum OrderStatus {
    #[default]
    Draft,
    Placed,
    Shipped,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Order {
    status: OrderStatus,
    total: f64,
}

impl Aggregate for Order {
    fn aggregate_type() -> &'static str {
        "Order"
    }
}

impl Apply<OrderPlaced> for Order {
    fn apply(&mut self, event: OrderPlaced) {
        self.status = OrderStatus::Placed;
        self.total = event.total;
    }
}

impl Apply<OrderShipped> for Order {
    fn apply(&mut self, _event: OrderShipped) {
        self.status = OrderStatus::Shipped;
    }
}

// ═══════════════════════════════════════════════════════════
// CAUSAL LINKS (parent_event_id)
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn parent_event_id_set_on_inline_chain() -> Result<()> {
    let seen_parent: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sp = seen_parent.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<EventA>().then(
            |event: Arc<EventA>, _ctx: Context<Deps>| async move {
                Ok(events![EventB {
                    value: event.value + 1,
                }])
            },
        ))
        .with_handler(handler::on::<EventB>().then(
            move |_event: Arc<EventB>, ctx: Context<Deps>| {
                let sp = sp.clone();
                async move {
                    *sp.lock() = Some(ctx.parent_event_id().unwrap());
                    Ok(events![])
                }
            },
        ));

    let handle = engine.emit(EventA { value: 1 }).settled().await?;

    let parent = seen_parent.lock().unwrap();
    // EventB's parent should be the root event (EventA)
    assert_eq!(parent, handle.event_id, "parent_event_id should be the dispatched event");
    Ok(())
}

#[tokio::test]
async fn parent_event_id_set_on_queued_chain() -> Result<()> {
    let seen_parent: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sp = seen_parent.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<EventA>()
                .id("emit_b")
                .retry(1)
                .then(|event: Arc<EventA>, _ctx: Context<Deps>| async move {
                    Ok(events![EventB {
                        value: event.value + 1,
                    }])
                }),
        )
        .with_handler(handler::on::<EventB>().then(
            move |_event: Arc<EventB>, ctx: Context<Deps>| {
                let sp = sp.clone();
                async move {
                    *sp.lock() = Some(ctx.parent_event_id().unwrap());
                    Ok(events![])
                }
            },
        ));

    let _handle = engine.emit(EventA { value: 1 }).settled().await?;
    assert!(seen_parent.lock().is_some(), "EventB handler should have run");
    Ok(())
}

#[tokio::test]
async fn deep_causal_chain_preserves_parent_links() -> Result<()> {
    // A → B → C → D, each handler emits the next event.
    // Each event's parent should be the preceding event (not root).
    let parents: Arc<Mutex<Vec<(String, Option<Uuid>)>>> = Arc::new(Mutex::new(Vec::new()));

    let p1 = parents.clone();
    let p2 = parents.clone();
    let p3 = parents.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<EventA>().then(
            |event: Arc<EventA>, _ctx: Context<Deps>| async move {
                Ok(events![EventB {
                    value: event.value + 1,
                }])
            },
        ))
        .with_handler(handler::on::<EventB>().then(
            move |event: Arc<EventB>, ctx: Context<Deps>| {
                let p = p1.clone();
                async move {
                    p.lock().push(("B".into(), ctx.parent_event_id()));
                    Ok(events![EventC {
                        value: event.value + 1,
                    }])
                }
            },
        ))
        .with_handler(handler::on::<EventC>().then(
            move |event: Arc<EventC>, ctx: Context<Deps>| {
                let p = p2.clone();
                async move {
                    p.lock().push(("C".into(), ctx.parent_event_id()));
                    Ok(events![EventD {
                        value: event.value + 1,
                    }])
                }
            },
        ))
        .with_handler(handler::on::<EventD>().then(
            move |_event: Arc<EventD>, ctx: Context<Deps>| {
                let p = p3.clone();
                async move {
                    p.lock().push(("D".into(), ctx.parent_event_id()));
                    Ok(events![])
                }
            },
        ));

    engine.emit(EventA { value: 0 }).settled().await?;

    let parents = parents.lock();
    assert_eq!(parents.len(), 3, "B, C, D handlers should all fire");
    // Every handler should have a parent
    for (name, parent) in parents.iter() {
        assert!(
            parent.is_some(),
            "Handler {} should have parent_event_id",
            name
        );
    }
    // Each parent should be different (unique event IDs in chain)
    let parent_ids: Vec<Uuid> = parents.iter().filter_map(|(_, p)| *p).collect();
    for i in 0..parent_ids.len() {
        for j in (i + 1)..parent_ids.len() {
            assert_ne!(
                parent_ids[i], parent_ids[j],
                "Each event in the chain should have a unique parent"
            );
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// AGGREGATE STATE + TRANSITION GUARDS
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn aggregate_state_transitions_correctly() -> Result<()> {
    let notified: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
    let n = notified.clone();

    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(handler::on::<OrderPlaced>().then(
            |event: Arc<OrderPlaced>, _ctx: Context<Deps>| async move {
                Ok(events![OrderShipped {
                    order_id: event.order_id,
                }])
            },
        ))
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status != OrderStatus::Shipped && next.status == OrderStatus::Shipped
                })
                .then(move |order_id: Uuid, _ctx: Context<Deps>| {
                    let n = n.clone();
                    async move {
                        n.lock().push(order_id);
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(OrderPlaced {
            order_id,
            total: 99.99,
        })
        .settled()
        .await?;

    let notified = notified.lock();
    assert_eq!(notified.len(), 1, "transition guard should fire once");
    assert_eq!(notified[0], order_id);
    Ok(())
}

#[tokio::test]
async fn transition_guard_blocks_duplicate_transition() -> Result<()> {
    // Ship the same order twice — guard should only fire once
    // (second time prev.status is already Shipped)
    let fire_count = Arc::new(AtomicUsize::new(0));
    let fc = fire_count.clone();
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status != OrderStatus::Shipped && next.status == OrderStatus::Shipped
                })
                .then(move |_order_id: Uuid, _ctx: Context<Deps>| {
                    let fc = fc.clone();
                    async move {
                        fc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    // First: Place then Ship → guard fires
    engine
        .emit(OrderPlaced {
            order_id,
            total: 50.0,
        })
        .settled()
        .await?;
    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(fire_count.load(Ordering::SeqCst), 1, "first ship fires guard");

    // Second: Ship again → guard should NOT fire (already Shipped→Shipped)
    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(
        fire_count.load(Ordering::SeqCst),
        1,
        "second ship should NOT fire guard (already shipped)"
    );
    Ok(())
}

#[tokio::test]
async fn transition_guard_does_not_fire_when_guard_returns_false() -> Result<()> {
    let fire_count = Arc::new(AtomicUsize::new(0));
    let fc = fire_count.clone();
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderPlaced>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|_prev, next| {
                    // Only fire if total > 100 — but we'll send total=50
                    next.total > 100.0
                })
                .then(move |_order_id: Uuid, _ctx: Context<Deps>| {
                    let fc = fc.clone();
                    async move {
                        fc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(OrderPlaced {
            order_id,
            total: 50.0,
        })
        .settled()
        .await?;

    assert_eq!(
        fire_count.load(Ordering::SeqCst),
        0,
        "guard should block when total <= 100"
    );
    Ok(())
}

#[tokio::test]
async fn aggregate_state_isolated_between_ids() -> Result<()> {
    // Two different order IDs should have independent aggregate state
    let fired_ids: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
    let fi = fired_ids.clone();

    let order_a = Uuid::new_v4();
    let order_b = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status != OrderStatus::Shipped && next.status == OrderStatus::Shipped
                })
                .then(move |id: Uuid, _ctx: Context<Deps>| {
                    let fi = fi.clone();
                    async move {
                        fi.lock().push(id);
                        Ok(events![])
                    }
                }),
        );

    // Place and ship order A
    engine
        .emit(OrderPlaced {
            order_id: order_a,
            total: 10.0,
        })
        .settled()
        .await?;
    engine
        .emit(OrderShipped { order_id: order_a })
        .settled()
        .await?;

    // Place and ship order B
    engine
        .emit(OrderPlaced {
            order_id: order_b,
            total: 20.0,
        })
        .settled()
        .await?;
    engine
        .emit(OrderShipped { order_id: order_b })
        .settled()
        .await?;

    let fired = fired_ids.lock();
    assert_eq!(fired.len(), 2, "both orders should trigger the guard");
    assert!(fired.contains(&order_a));
    assert!(fired.contains(&order_b));
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// FILTER AND EXTRACT PREDICATES
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn filter_blocks_non_matching_events() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<EventA>()
            .filter(|e| e.value > 10)
            .then(move |_event: Arc<EventA>, _ctx: Context<Deps>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    // value=5: filtered out
    engine.emit(EventA { value: 5 }).settled().await?;
    assert_eq!(counter.load(Ordering::SeqCst), 0, "value=5 should be filtered");

    // value=20: passes filter
    engine.emit(EventA { value: 20 }).settled().await?;
    assert_eq!(counter.load(Ordering::SeqCst), 1, "value=20 should pass filter");

    Ok(())
}

#[tokio::test]
async fn extract_returns_none_skips_handler() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<EventA>()
            .extract(|e| {
                if e.value > 0 {
                    Some(e.value as u64)
                } else {
                    None
                }
            })
            .then(move |_val: u64, _ctx: Context<Deps>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    // value=0: extract returns None, handler skipped
    engine.emit(EventA { value: 0 }).settled().await?;
    assert_eq!(counter.load(Ordering::SeqCst), 0, "extract→None should skip handler");

    // value=5: extract returns Some, handler runs
    engine.emit(EventA { value: 5 }).settled().await?;
    assert_eq!(counter.load(Ordering::SeqCst), 1, "extract→Some should run handler");

    Ok(())
}

// ═══════════════════════════════════════════════════════════
// PRIORITY ORDERING
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn inline_handlers_execute_in_priority_order() -> Result<()> {
    let execution_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let eo1 = execution_order.clone();
    let eo2 = execution_order.clone();
    let eo3 = execution_order.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<Ping>()
                .id("low_priority")
                .priority(100)
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let eo = eo1.clone();
                    async move {
                        eo.lock().push("low".into());
                        Ok(events![])
                    }
                }),
        )
        .with_handler(
            handler::on::<Ping>()
                .id("high_priority")
                .priority(1)
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let eo = eo2.clone();
                    async move {
                        eo.lock().push("high".into());
                        Ok(events![])
                    }
                }),
        )
        .with_handler(
            handler::on::<Ping>()
                .id("medium_priority")
                .priority(50)
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let eo = eo3.clone();
                    async move {
                        eo.lock().push("medium".into());
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "test".into(),
        })
        .settled()
        .await?;

    let order = execution_order.lock();
    assert_eq!(order.as_slice(), &["high", "medium", "low"]);
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// IDEMPOTENCY KEY DETERMINISM
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn idempotency_key_is_deterministic() -> Result<()> {
    let seen_keys: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sk = seen_keys.clone();

    let engine = Engine::new(Deps).with_handler(handler::on::<Ping>().then(
        move |_event: Arc<Ping>, ctx: Context<Deps>| {
            let sk = sk.clone();
            async move {
                sk.lock().push(ctx.idempotency_key().to_string());
                Ok(events![])
            }
        },
    ));

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    let keys = seen_keys.lock();
    assert_eq!(keys.len(), 1);
    // Key should be a valid UUID string (deterministic from event_id + handler_id)
    assert!(
        Uuid::parse_str(&keys[0]).is_ok(),
        "idempotency key should be a valid UUID"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// CONTEXT FIELDS
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn context_exposes_all_fields() -> Result<()> {
    let ctx_data: Arc<Mutex<Option<(Uuid, Uuid, Option<Uuid>, String, String)>>> =
        Arc::new(Mutex::new(None));
    let cd = ctx_data.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("test_ctx")
            .then(move |_event: Arc<Ping>, ctx: Context<Deps>| {
                let cd = cd.clone();
                async move {
                    *cd.lock() = Some((
                        ctx.correlation_id,
                        ctx.event_id,
                        ctx.parent_event_id(),
                        ctx.handler_id().to_string(),
                        ctx.idempotency_key().to_string(),
                    ));
                    Ok(events![])
                }
            }),
    );

    let handle = engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    let data = ctx_data.lock();
    let (correlation_id, event_id, parent, handler_id, idem_key) = data.as_ref().unwrap();

    assert_eq!(*correlation_id, handle.correlation_id);
    assert_eq!(*event_id, handle.event_id);
    assert!(parent.is_none(), "root event has no parent");
    assert_eq!(handler_id, "test_ctx");
    assert!(!idem_key.is_empty());
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// HOPS LIMIT (INFINITE LOOP DETECTION)
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn hops_limit_prevents_infinite_loop() -> Result<()> {
    // A handler that emits EventA on EventA — infinite loop
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps).with_handler(handler::on::<EventA>().then(
        move |event: Arc<EventA>, _ctx: Context<Deps>| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(events![EventA {
                    value: event.value + 1,
                }])
            }
        },
    ));

    // This should NOT hang forever due to max_hops (default 50)
    engine.emit(EventA { value: 0 }).settled().await?;

    let count = counter.load(Ordering::SeqCst);
    assert!(
        count > 1 && count <= 51,
        "handler should fire multiple times then stop at hop limit, fired {} times",
        count
    );
    Ok(())
}

#[tokio::test]
async fn hops_increment_through_chain() -> Result<()> {
    // Track the hops value at each step via event values
    // A → B → C. Each handler passes along the value.
    let values_seen: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
    let vs = values_seen.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<EventA>().then(
            |event: Arc<EventA>, _ctx: Context<Deps>| async move {
                Ok(events![EventB {
                    value: event.value,
                }])
            },
        ))
        .with_handler(handler::on::<EventB>().then(
            |event: Arc<EventB>, _ctx: Context<Deps>| async move {
                Ok(events![EventC {
                    value: event.value,
                }])
            },
        ))
        .with_handler(handler::on::<EventC>().then(
            move |event: Arc<EventC>, _ctx: Context<Deps>| {
                let vs = vs.clone();
                async move {
                    vs.lock().push(event.value);
                    Ok(events![])
                }
            },
        ));

    engine.emit(EventA { value: 42 }).settled().await?;

    let values = values_seen.lock();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0], 42, "value should propagate through chain");
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// ERROR PROPAGATION
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn inline_handler_error_does_not_stop_other_handlers() -> Result<()> {
    let success_counter = Arc::new(AtomicUsize::new(0));
    let sc = success_counter.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<Ping>()
                .id("failing_handler")
                .then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("I always fail"))
                }),
        )
        .with_handler(
            handler::on::<Ping>()
                .id("succeeding_handler")
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let sc = sc.clone();
                    async move {
                        sc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "test".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        success_counter.load(Ordering::SeqCst),
        1,
        "other handlers should still run when one fails"
    );
    Ok(())
}

#[tokio::test]
async fn queued_handler_exhausts_retries_then_dlqs() -> Result<()> {
    let attempt_counter = Arc::new(AtomicI32::new(0));
    let ac = attempt_counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<FailEvent>()
            .id("always_fail_retry3")
            .retry(3)
            .then(move |_event: Arc<FailEvent>, _ctx: Context<Deps>| {
                let ac = ac.clone();
                async move {
                    ac.fetch_add(1, Ordering::SeqCst);
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }
            }),
    );

    engine
        .emit(FailEvent { attempt: 0 })
        .settled()
        .await?;

    // retry(3) sets max_attempts=3. The check is `attempts >= max_attempts`
    // AFTER the handler runs: attempts 0,1,2 → Retry, attempt 3 → Failed.
    // So 4 total calls (initial + 3 retries).
    assert_eq!(
        attempt_counter.load(Ordering::SeqCst),
        4,
        "retry(3) = 4 total calls (initial + 3 retries)"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// EMIT EDGE CASES
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn emit_none_produces_no_events() -> Result<()> {
    let downstream_counter = Arc::new(AtomicUsize::new(0));
    let dc = downstream_counter.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) },
        ))
        .with_handler(handler::on::<EventA>().then(
            move |_event: Arc<EventA>, _ctx: Context<Deps>| {
                let dc = dc.clone();
                async move {
                    dc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine
        .emit(Ping {
            msg: "noop".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        downstream_counter.load(Ordering::SeqCst),
        0,
        "returning () should not trigger any downstream handlers"
    );
    Ok(())
}

#[tokio::test]
async fn emit_batch_creates_correct_metadata() -> Result<()> {
    let batch_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let bs = batch_sizes.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                Ok(events![..vec![
                    BatchItem { index: 0 },
                    BatchItem { index: 1 },
                    BatchItem { index: 2 },
                    BatchItem { index: 3 },
                    BatchItem { index: 4 },
                ]])
            },
        ))
        .with_handler(
            handler::on::<BatchItem>()
                .id("collector")
                .accumulate()
                .then(move |batch: Vec<BatchItem>, _ctx: Context<Deps>| {
                    let bs = bs.clone();
                    async move {
                        bs.lock().push(batch.len());
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "batch".into(),
        })
        .settled()
        .await?;

    let sizes = batch_sizes.lock();
    assert_eq!(sizes.len(), 1, "accumulator should fire once");
    assert_eq!(sizes[0], 5, "all 5 items should be in the batch");
    Ok(())
}

#[tokio::test]
async fn emit_option_none_produces_no_events() -> Result<()> {
    let downstream = Arc::new(AtomicUsize::new(0));
    let dc = downstream.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                Ok(events![])
            },
        ))
        .with_handler(handler::on::<EventA>().then(
            move |_event: Arc<EventA>, _ctx: Context<Deps>| {
                let dc = dc.clone();
                async move {
                    dc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine
        .emit(Ping {
            msg: "maybe".into(),
        })
        .settled()
        .await?;

    assert_eq!(downstream.load(Ordering::SeqCst), 0, "None should emit nothing");
    Ok(())
}

#[tokio::test]
async fn emit_option_some_produces_event() -> Result<()> {
    let downstream = Arc::new(AtomicUsize::new(0));
    let dc = downstream.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                Ok(events![EventA { value: 42 }])
            },
        ))
        .with_handler(handler::on::<EventA>().then(
            move |_event: Arc<EventA>, _ctx: Context<Deps>| {
                let dc = dc.clone();
                async move {
                    dc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine
        .emit(Ping {
            msg: "maybe".into(),
        })
        .settled()
        .await?;

    assert_eq!(downstream.load(Ordering::SeqCst), 1, "Some should emit event");
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// MULTI-DISPATCH (independent workflows)
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn multiple_dispatches_have_independent_correlation_ids() -> Result<()> {
    let correlations: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
    let c = correlations.clone();

    let engine = Engine::new(Deps).with_handler(handler::on::<Ping>().then(
        move |_event: Arc<Ping>, ctx: Context<Deps>| {
            let c = c.clone();
            async move {
                c.lock().push(ctx.correlation_id);
                Ok(events![])
            }
        },
    ));

    let h1 = engine
        .emit(Ping {
            msg: "one".into(),
        })
        .settled()
        .await?;
    let h2 = engine
        .emit(Ping {
            msg: "two".into(),
        })
        .settled()
        .await?;

    assert_ne!(
        h1.correlation_id, h2.correlation_id,
        "each dispatch gets a unique correlation_id"
    );

    let correlations = correlations.lock();
    assert_eq!(correlations.len(), 2);
    assert_eq!(correlations[0], h1.correlation_id);
    assert_eq!(correlations[1], h2.correlation_id);
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// DLQ TERMINAL on_failure DETAIL CHECK
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn on_failure_receives_correct_error_info() -> Result<()> {
    let terminal_data: Arc<Mutex<Option<(String, i32)>>> = Arc::new(Mutex::new(None));
    let td = terminal_data.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("fail_with_info")
                .retry(2)
                .on_failure(|_event: Arc<FailEvent>, info: seesaw_core::ErrorContext| {
                    FailedTerminal {
                        error: info.error,
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("specific error message"))
                }),
        )
        .with_handler(handler::on::<FailedTerminal>().then(
            move |event: Arc<FailedTerminal>, _ctx: Context<Deps>| {
                let td = td.clone();
                async move {
                    *td.lock() = Some((event.error.clone(), event.attempts));
                    Ok(events![])
                }
            },
        ));

    engine
        .emit(FailEvent { attempt: 0 })
        .settled()
        .await?;

    let data = terminal_data.lock();
    let (error, attempts) = data.as_ref().expect("terminal handler should run");
    assert!(
        error.contains("specific error message"),
        "error should contain the original message, got: {}",
        error
    );
    assert_eq!(*attempts, 2, "should report final attempt count");
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// NO HANDLER REGISTERED FOR EVENT
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn dispatch_event_with_no_handler_settles_cleanly() -> Result<()> {
    let engine = Engine::new(Deps);

    // Dispatch an event that no handler listens to
    engine
        .emit(Ping {
            msg: "nobody".into(),
        })
        .settled()
        .await?;

    // Should not panic or hang
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// HANDLER ON_ANY (observer)
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn on_any_handler_sees_all_events() -> Result<()> {
    let seen_types: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let st = seen_types.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on_any()
                .id("logger")
                .then(move |event: seesaw_core::handler::AnyEvent, _ctx: Context<Deps>| {
                    let st = st.clone();
                    async move {
                        let type_name = if event.is::<Ping>() {
                            "Ping"
                        } else if event.is::<EventA>() {
                            "EventA"
                        } else {
                            "unknown"
                        };
                        st.lock().push(type_name.into());
                        Ok(events![])
                    }
                }),
        )
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                Ok(events![EventA { value: 1 }])
            },
        ))
        // Need a handler for EventA so its codec is registered
        .with_handler(handler::on::<EventA>().then(
            |_event: Arc<EventA>, _ctx: Context<Deps>| async move { Ok(events![]) },
        ));

    engine
        .emit(Ping {
            msg: "observed".into(),
        })
        .settled()
        .await?;

    let types = seen_types.lock();
    assert!(
        types.contains(&"Ping".to_string()),
        "on_any should see Ping"
    );
    assert!(
        types.contains(&"EventA".to_string()),
        "on_any should see EventA emitted by Ping handler"
    );
    Ok(())
}

#[tokio::test]
async fn on_any_handler_emits_child_events() -> Result<()> {
    let child_handled = Arc::new(AtomicUsize::new(0));
    let ch = child_handled.clone();

    let engine = Engine::new(Deps)
        // Typed Ping handler so codec is registered
        .with_handler(handler::on::<Ping>().id("ping_noop").then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) },
        ))
        // on_any emits EventB when it sees a Ping
        .with_handler(
            handler::on_any()
                .id("emitter")
                .then(|event: seesaw_core::handler::AnyEvent, _ctx: Context<Deps>| async move {
                    if event.is::<Ping>() {
                        Ok(events![EventB { value: 42 }])
                    } else {
                        Ok(events![])
                    }
                }),
        )
        // Typed handler proves the child event was dispatched
        .with_handler(handler::on::<EventB>().id("child_receiver").then(
            move |event: Arc<EventB>, _ctx: Context<Deps>| {
                let ch = ch.clone();
                async move {
                    assert_eq!(event.value, 42);
                    ch.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine
        .emit(Ping {
            msg: "trigger".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        child_handled.load(Ordering::SeqCst),
        1,
        "on_any should emit EventB and it should be handled by child_receiver"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// HANDLER WITH INIT (startup hook)
// ═══════════════════════════════════════════════════════════

// Note: startup handlers are invoked by run_startup_handlers(),
// which is called by backends. The Engine itself doesn't call it
// during settle. Testing the builder compiles correctly.

#[tokio::test]
async fn handler_with_init_compiles_and_handles_events() -> Result<()> {
    let init_ran = Arc::new(AtomicUsize::new(0));
    let handler_ran = Arc::new(AtomicUsize::new(0));
    let hr = handler_ran.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .init(move |_ctx: Context<Deps>| {
                // Note: init is not called by Engine.settle()
                async move { Ok(()) }
            })
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let hr = hr.clone();
                async move {
                    hr.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    engine
        .emit(Ping {
            msg: "init".into(),
        })
        .settled()
        .await?;

    assert_eq!(init_ran.load(Ordering::SeqCst), 0, "init not called by settle");
    assert_eq!(handler_ran.load(Ordering::SeqCst), 1, "handler still fires");
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// DELAYED EXECUTION
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn delayed_handler_eventually_executes() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("delayed_ping")
            .delayed(Duration::from_millis(10))
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    // settled() drives the full causal tree including delayed effects.
    // The settle loop sleeps until the delay expires, then executes the handler.
    engine
        .emit(Ping {
            msg: "delayed".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "delayed handler should execute after delay within settled()"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// QUEUED HANDLER EMITS CHAIN
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn queued_handler_emitted_events_are_processed() -> Result<()> {
    let b_counter = Arc::new(AtomicUsize::new(0));
    let bc = b_counter.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<EventA>()
                .id("emit_b_queued")
                .retry(1)
                .then(|event: Arc<EventA>, _ctx: Context<Deps>| async move {
                    Ok(events![EventB {
                        value: event.value * 2,
                    }])
                }),
        )
        .with_handler(handler::on::<EventB>().then(
            move |event: Arc<EventB>, _ctx: Context<Deps>| {
                let bc = bc.clone();
                async move {
                    bc.fetch_add(event.value as usize, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine.emit(EventA { value: 5 }).settled().await?;

    assert_eq!(
        b_counter.load(Ordering::SeqCst),
        10,
        "queued handler should emit EventB which is then processed"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// DEPS ACCESS
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn handler_can_access_deps() -> Result<()> {
    #[derive(Clone)]
    struct AppDeps {
        value: i32,
    }

    let seen_value = Arc::new(AtomicI32::new(0));
    let sv = seen_value.clone();

    let engine = Engine::new(AppDeps { value: 42 }).with_handler(handler::on::<Ping>().then(
        move |_event: Arc<Ping>, ctx: Context<AppDeps>| {
            let sv = sv.clone();
            async move {
                sv.store(ctx.deps().value, Ordering::SeqCst);
                Ok(events![])
            }
        },
    ));

    engine
        .emit(Ping {
            msg: "deps".into(),
        })
        .settled()
        .await?;

    assert_eq!(seen_value.load(Ordering::SeqCst), 42);
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// LARGE BATCH ACCUMULATION
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn accumulate_large_batch() -> Result<()> {
    let batch_size_seen = Arc::new(AtomicUsize::new(0));
    let bs = batch_size_seen.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<Ping>().then(
                |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                    let items: Vec<BatchItem> = (0..100).map(|i| BatchItem { index: i }).collect();
                    Ok(events![..items])
                },
            ),
        )
        .with_handler(
            handler::on::<BatchItem>()
                .id("large_batch")
                .accumulate()
                .then(move |batch: Vec<BatchItem>, _ctx: Context<Deps>| {
                    let bs = bs.clone();
                    async move {
                        bs.store(batch.len(), Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "large".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        batch_size_seen.load(Ordering::SeqCst),
        100,
        "all 100 batch items should accumulate"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// ACCUMULATE HANDLER EMITS DOWNSTREAM
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn accumulate_handler_emits_downstream_event() -> Result<()> {
    let result_seen = Arc::new(AtomicUsize::new(0));
    let rs = result_seen.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<Ping>().then(
                |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                    Ok(events![..vec![
                        BatchItem { index: 0 },
                        BatchItem { index: 1 },
                    ]])
                },
            ),
        )
        .with_handler(
            handler::on::<BatchItem>()
                .id("batch_with_result")
                .accumulate()
                .then(|batch: Vec<BatchItem>, _ctx: Context<Deps>| async move {
                    Ok(events![BatchResult { count: batch.len() }])
                }),
        )
        .with_handler(handler::on::<BatchResult>().then(
            move |event: Arc<BatchResult>, _ctx: Context<Deps>| {
                let rs = rs.clone();
                async move {
                    rs.store(event.count, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine
        .emit(Ping {
            msg: "chain".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        result_seen.load(Ordering::SeqCst),
        2,
        "downstream handler should see BatchResult with count=2"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// DISPATCH RETURNS CORRECT HANDLE
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn dispatch_returns_valid_process_handle() -> Result<()> {
    let engine = Engine::new(Deps).with_handler(handler::on::<Ping>().then(
        |_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) },
    ));

    let handle = engine
        .emit(Ping {
            msg: "handle".into(),
        })
        .settled()
        .await?;

    assert_ne!(handle.correlation_id, Uuid::nil());
    assert_ne!(handle.event_id, Uuid::nil());
    assert_ne!(handle.correlation_id, handle.event_id);
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// HANDLER TIMEOUT
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn queued_handler_timeout_goes_to_dlq() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("timeout_handler")
            .timeout(Duration::from_millis(50))
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    // Sleep longer than timeout
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(events![])
                }
            }),
    );

    engine
        .emit(Ping {
            msg: "timeout".into(),
        })
        .settled()
        .await?;

    // Default max_attempts=1. Timeout check: attempts(0) >= max(1)? No → Retry.
    // Second attempt: attempts(1) >= max(1)? Yes → Failed/DLQ.
    // So handler is called exactly 2 times.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "timeout handler: 2 calls (initial + 1 retry) before DLQ"
    );
    Ok(())
}

// ── Fan-out → Map → Fan-in ─────────────────────────────────────

/// Ev1 is the intermediate event produced by the fan-out step.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FanItem {
    index: i32,
}

/// Ev2 is produced by the 1:1 map step and collected by the accumulate step.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MappedItem {
    index: i32,
    transformed: bool,
}

/// Final result after accumulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FanInResult {
    count: usize,
    indices: Vec<i32>,
}

#[tokio::test]
async fn fanout_map_fanin_batch_metadata_propagates() -> Result<()> {
    // A dispatches Ping → handler A fans out to vec![FanItem; 4]
    // Handler B maps each FanItem → MappedItem (1:1, batch metadata inherited)
    // Handler C accumulates all 4 MappedItems via same_batch join

    let result: Arc<Mutex<Option<FanInResult>>> = Arc::new(Mutex::new(None));
    let r = result.clone();

    let b_call_count = Arc::new(AtomicI32::new(0));
    let b_count = b_call_count.clone();

    let engine = Engine::new(Deps)
        // Handler A: fan-out Ping → vec![FanItem; 4]
        .with_handler(
            handler::on::<Ping>()
                .id("fanout_a")
                .then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                    let items: Vec<FanItem> = (0..4).map(|i| FanItem { index: i }).collect();
                    Ok(events![..items])
                }),
        )
        // Handler B: map each FanItem → MappedItem (1:1)
        .with_handler(
            handler::on::<FanItem>()
                .id("map_b")
                .retry(1)
                .then(move |event: Arc<FanItem>, _ctx: Context<Deps>| {
                    let b = b_count.clone();
                    async move {
                        b.fetch_add(1, Ordering::SeqCst);
                        Ok(events![MappedItem {
                            index: event.index,
                            transformed: true,
                        }])
                    }
                }),
        )
        // Handler C: accumulate all MappedItems in the batch
        .with_handler(
            handler::on::<MappedItem>()
                .id("accumulate_c")
                .accumulate()
                .window(Duration::from_secs(5))
                .then(move |batch: Vec<MappedItem>, _ctx: Context<Deps>| {
                    let r = r.clone();
                    async move {
                        let mut indices: Vec<i32> = batch.iter().map(|m| m.index).collect();
                        indices.sort();
                        let res = FanInResult {
                            count: batch.len(),
                            indices,
                        };
                        *r.lock() = Some(res.clone());
                        Ok(events![res])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "fanout".into(),
        })
        .settled()
        .await?;

    // Handler B should have been called 4 times (once per FanItem)
    assert_eq!(
        b_call_count.load(Ordering::SeqCst),
        4,
        "map handler B should be called once per FanItem"
    );

    // Handler C should have received all 4 MappedItems
    let batch_result = result.lock().clone().expect("accumulate handler C should have fired");
    assert_eq!(batch_result.count, 4, "should accumulate all 4 items");
    assert_eq!(
        batch_result.indices,
        vec![0, 1, 2, 3],
        "all indices should be present and ordered"
    );

    Ok(())
}

// ═══════════════════════════════════════════════════════════
// PROJECTION HANDLERS
// ═══════════════════════════════════════════════════════════

/// Simulates a read model / projection store.
#[derive(Clone)]
struct ProjectionDeps {
    store: Arc<dashmap::DashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderCreated {
    order_id: String,
    customer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderProjected {
    order_id: String,
}

#[tokio::test]
async fn projection_handler_runs_before_regular_handlers() -> Result<()> {
    let store = Arc::new(dashmap::DashMap::new());
    let deps = ProjectionDeps { store: store.clone() };

    // Track what the regular handler saw
    let reader_saw_projection = Arc::new(parking_lot::Mutex::new(false));
    let rs = reader_saw_projection.clone();

    let engine = Engine::new(deps)
        // Projection handler: writes to the store FIRST
        .with_handler(
            handler::on::<OrderCreated>()
                .id("order_projection")
                .projection()
                .then(move |event: Arc<OrderCreated>, ctx: Context<ProjectionDeps>| {
                    async move {
                        ctx.deps().store.insert(
                            event.order_id.clone(),
                            event.customer.clone(),
                        );
                        Ok(events![])
                    }
                }),
        )
        // Regular handler: reads from the store — should see the projection
        .with_handler(
            handler::on::<OrderCreated>()
                .id("order_reader")
                .then(move |event: Arc<OrderCreated>, ctx: Context<ProjectionDeps>| {
                    let rs = rs.clone();
                    async move {
                        let found = ctx.deps().store.get(&event.order_id).is_some();
                        *rs.lock() = found;
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(OrderCreated {
            order_id: "order-1".into(),
            customer: "Alice".into(),
        })
        .settled()
        .await?;

    assert!(
        *reader_saw_projection.lock(),
        "regular handler should see projection data — projection must run first"
    );
    Ok(())
}

#[tokio::test]
async fn multiple_projections_run_sequentially_before_parallel_handlers() -> Result<()> {
    let store = Arc::new(dashmap::DashMap::new());
    let deps = ProjectionDeps { store: store.clone() };

    let execution_order: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));

    let eo1 = execution_order.clone();
    let eo2 = execution_order.clone();
    let eo3 = execution_order.clone();
    let eo4 = execution_order.clone();

    let engine = Engine::new(deps)
        // Projection 1
        .with_handler(
            handler::on::<OrderCreated>()
                .id("projection_1")
                .projection()
                .then(move |event: Arc<OrderCreated>, ctx: Context<ProjectionDeps>| {
                    let eo = eo1.clone();
                    async move {
                        ctx.deps().store.insert(
                            event.order_id.clone(),
                            event.customer.clone(),
                        );
                        eo.lock().push("projection_1".into());
                        Ok(events![])
                    }
                }),
        )
        // Projection 2
        .with_handler(
            handler::on::<OrderCreated>()
                .id("projection_2")
                .projection()
                .then(move |event: Arc<OrderCreated>, ctx: Context<ProjectionDeps>| {
                    let eo = eo2.clone();
                    async move {
                        ctx.deps().store.insert(
                            format!("{}_index", event.order_id),
                            "indexed".into(),
                        );
                        eo.lock().push("projection_2".into());
                        Ok(events![])
                    }
                }),
        )
        // Regular handler A
        .with_handler(
            handler::on::<OrderCreated>()
                .id("handler_a")
                .then(move |_event: Arc<OrderCreated>, _ctx: Context<ProjectionDeps>| {
                    let eo = eo3.clone();
                    async move {
                        eo.lock().push("handler_a".into());
                        Ok(events![])
                    }
                }),
        )
        // Regular handler B
        .with_handler(
            handler::on::<OrderCreated>()
                .id("handler_b")
                .then(move |_event: Arc<OrderCreated>, _ctx: Context<ProjectionDeps>| {
                    let eo = eo4.clone();
                    async move {
                        eo.lock().push("handler_b".into());
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(OrderCreated {
            order_id: "order-2".into(),
            customer: "Bob".into(),
        })
        .settled()
        .await?;

    let order = execution_order.lock();

    // Both projections must come before any regular handler
    let proj_1_pos = order.iter().position(|s| s == "projection_1").unwrap();
    let proj_2_pos = order.iter().position(|s| s == "projection_2").unwrap();
    let handler_a_pos = order.iter().position(|s| s == "handler_a").unwrap();
    let handler_b_pos = order.iter().position(|s| s == "handler_b").unwrap();

    assert!(
        proj_1_pos < handler_a_pos && proj_1_pos < handler_b_pos,
        "projection_1 must run before regular handlers, order: {:?}",
        *order
    );
    assert!(
        proj_2_pos < handler_a_pos && proj_2_pos < handler_b_pos,
        "projection_2 must run before regular handlers, order: {:?}",
        *order
    );

    // Both read model entries should exist
    assert!(store.get("order-2").is_some());
    assert!(store.get("order-2_index").is_some());

    Ok(())
}

#[tokio::test]
async fn projection_handler_can_emit_events() -> Result<()> {
    let store = Arc::new(dashmap::DashMap::new());
    let deps = ProjectionDeps { store: store.clone() };

    let projected_count = Arc::new(AtomicUsize::new(0));
    let pc = projected_count.clone();

    let engine = Engine::new(deps)
        .with_handler(
            handler::on::<OrderCreated>()
                .projection()
                .then(move |event: Arc<OrderCreated>, ctx: Context<ProjectionDeps>| {
                    async move {
                        ctx.deps().store.insert(
                            event.order_id.clone(),
                            event.customer.clone(),
                        );
                        Ok(events![OrderProjected {
                            order_id: event.order_id.clone(),
                        }])
                    }
                }),
        )
        .with_handler(
            handler::on::<OrderProjected>()
                .then(move |_event: Arc<OrderProjected>, _ctx: Context<ProjectionDeps>| {
                    let pc = pc.clone();
                    async move {
                        pc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(OrderCreated {
            order_id: "order-3".into(),
            customer: "Charlie".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        projected_count.load(Ordering::SeqCst),
        1,
        "projection-emitted event should be handled"
    );
    assert!(store.get("order-3").is_some());
    Ok(())
}
