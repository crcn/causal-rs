//! Integration tests for Engine settle loop.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use seesaw_core::aggregator::{Aggregate, Apply};
use seesaw_core::{events, handler, Context, Engine, Events, MemoryStore, NewEvent, Snapshot, Store};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Helper to build a NewEvent with sensible defaults for tests.
fn new_event(
    event_type: &str,
    payload: serde_json::Value,
    aggregate_type: Option<&str>,
    aggregate_id: Option<Uuid>,
) -> NewEvent {
    NewEvent {
        event_id: Uuid::new_v4(),
        parent_id: None,
        correlation_id: Uuid::new_v4(),
        event_type: event_type.to_string(),
        payload,
        created_at: chrono::Utc::now(),
        aggregate_type: aggregate_type.map(|s| s.to_string()),
        aggregate_id,
        metadata: serde_json::Map::new(),
    }
}

#[derive(Clone)]
struct Deps;

// -- Event types --

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

// -- Tests --

#[tokio::test]
async fn basic_handler_fires() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(events![])
            }
        }),
    );

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn handler_emits_chain() -> Result<()> {
    let b_counter = Arc::new(AtomicUsize::new(0));
    let b_counter_clone = b_counter.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<EventA>().then(
            |event: Arc<EventA>, _ctx: Context<Deps>| async move {
                Ok(events![EventB {
                    value: event.value + 1,
                }])
            },
        ))
        .with_handler(handler::on::<EventB>().then(
            move |event: Arc<EventB>, _ctx: Context<Deps>| {
                let c = b_counter_clone.clone();
                async move {
                    c.fetch_add(event.value as usize, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine.emit(EventA { value: 10 }).settled().await?;

    assert_eq!(b_counter.load(Ordering::SeqCst), 11);
    Ok(())
}

#[tokio::test]
async fn multiple_handlers_same_event() -> Result<()> {
    let counter_a = Arc::new(AtomicUsize::new(0));
    let counter_b = Arc::new(AtomicUsize::new(0));
    let ca = counter_a.clone();
    let cb = counter_b.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<Ping>()
                .id("ping_handler_a")
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let c = ca.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        )
        .with_handler(
            handler::on::<Ping>()
                .id("ping_handler_b")
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let c = cb.clone();
                    async move {
                        c.fetch_add(10, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert_eq!(counter_a.load(Ordering::SeqCst), 1);
    assert_eq!(counter_b.load(Ordering::SeqCst), 10);
    Ok(())
}

#[tokio::test]
async fn queued_handler_executes() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("queued_ping")
            .retry(1)
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn emit_requires_settled() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("queued_fire_forget")
            .retry(1)
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    // Fire-and-forget: emit without settled
    let _handle = engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .await?;

    // Queued handler has not run yet (no settle)
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // Now settle
    engine.settle().await?;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn handler_returns_nothing() -> Result<()> {
    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
            Ok(events![])
        }),
    );

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    Ok(())
}

#[tokio::test]
async fn retry_succeeds_on_second_attempt() -> Result<()> {
    let attempt_counter = Arc::new(AtomicI32::new(0));
    let success_counter = Arc::new(AtomicUsize::new(0));
    let ac = attempt_counter.clone();
    let sc = success_counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<FailEvent>()
            .id("retry_handler")
            .retry(3)
            .then(move |_event: Arc<FailEvent>, _ctx: Context<Deps>| {
                let ac = ac.clone();
                let sc = sc.clone();
                async move {
                    let attempt = ac.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        anyhow::bail!("first attempt fails");
                    }
                    sc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    engine
        .emit(FailEvent { attempt: 0 })
        .settled()
        .await?;

    assert!(
        attempt_counter.load(Ordering::SeqCst) >= 2,
        "should have been called at least twice"
    );
    assert_eq!(
        success_counter.load(Ordering::SeqCst),
        1,
        "should succeed on retry"
    );
    Ok(())
}

#[tokio::test]
async fn dlq_terminal_event_published() -> Result<()> {
    let terminal_counter = Arc::new(AtomicUsize::new(0));
    let tc = terminal_counter.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("always_fail")
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: seesaw_core::ErrorContext| {
                    FailedTerminal {
                        error: info.error,
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }),
        )
        .with_handler(handler::on::<FailedTerminal>().then(
            move |_event: Arc<FailedTerminal>, _ctx: Context<Deps>| {
                let c = tc.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine
        .emit(FailEvent { attempt: 0 })
        .settled()
        .await?;

    assert_eq!(
        terminal_counter.load(Ordering::SeqCst),
        1,
        "terminal event from on_failure should be published and handled"
    );
    Ok(())
}

#[tokio::test]
async fn accumulate_batch() -> Result<()> {
    let result_counter = Arc::new(AtomicUsize::new(0));
    let batch_size_seen = Arc::new(AtomicUsize::new(0));
    let rc = result_counter.clone();
    let bs = batch_size_seen.clone();

    // An inline handler emits a batch which sets batch metadata automatically.
    // The accumulate handler collects all items in the batch.
    let engine = Engine::new(Deps)
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                Ok(events![..vec![
                    BatchItem { index: 0 },
                    BatchItem { index: 1 },
                    BatchItem { index: 2 },
                ]])
            },
        ))
        .with_handler(
            handler::on::<BatchItem>()
                .id("batch_accumulator")
                .accumulate()
                .then(move |batch: Vec<BatchItem>, _ctx: Context<Deps>| {
                    let rc = rc.clone();
                    let bs = bs.clone();
                    async move {
                        bs.store(batch.len(), Ordering::SeqCst);
                        rc.fetch_add(1, Ordering::SeqCst);
                        Ok(events![BatchResult { count: batch.len() }])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "trigger".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        result_counter.load(Ordering::SeqCst),
        1,
        "batch handler should fire exactly once"
    );
    assert_eq!(
        batch_size_seen.load(Ordering::SeqCst),
        3,
        "batch should contain all 3 items"
    );
    Ok(())
}

#[tokio::test]
async fn correlation_preserved_through_queued_chain() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<EventA>()
                .id("emit_b_queued")
                .retry(1)
                .then(|event: Arc<EventA>, _ctx: Context<Deps>| async move {
                    Ok(events![EventB {
                        value: event.value + 1,
                    }])
                }),
        )
        .with_handler(handler::on::<EventB>().then(
            move |_event: Arc<EventB>, ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    *sc.lock() = Some(ctx.correlation_id);
                    Ok(events![])
                }
            },
        ));

    let handle = engine.emit(EventA { value: 1 }).settled().await?;

    let seen = seen_correlation.lock().expect("EventB handler should have run");
    assert_eq!(
        seen, handle.correlation_id,
        "EventB emitted by queued handler must carry the original correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn dlq_terminal_preserves_correlation() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("always_fail_corr")
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: seesaw_core::ErrorContext| {
                    FailedTerminal {
                        error: info.error,
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }),
        )
        .with_handler(handler::on::<FailedTerminal>().then(
            move |_event: Arc<FailedTerminal>, ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    *sc.lock() = Some(ctx.correlation_id);
                    Ok(events![])
                }
            },
        ));

    let handle = engine
        .emit(FailEvent { attempt: 0 })
        .settled()
        .await?;

    let seen = seen_correlation
        .lock()
        .expect("FailedTerminal handler should have run");
    assert_eq!(
        seen, handle.correlation_id,
        "DLQ terminal event must carry the original correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn ctx_run_executes_side_effect_in_handler() -> Result<()> {
    let captured = Arc::new(Mutex::new(None::<String>));
    let cap = captured.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>().then(move |event: Arc<Ping>, ctx: Context<Deps>| {
            let cap = cap.clone();
            async move {
                let result: String = ctx
                    .run(move || async move { Ok(format!("processed-{}", event.msg)) })
                    .await?;
                *cap.lock() = Some(result);
                Ok(events![])
            }
        }),
    );

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        *captured.lock(),
        Some("processed-hello".to_string()),
        "ctx.run() should execute and return the side effect result"
    );
    Ok(())
}

// -- Upcaster tests --

/// Event with the "current" schema (v2) that includes a currency field.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    total: u64,
    currency: String,
}

#[tokio::test]
async fn upcaster_transforms_old_event_for_handler() -> Result<()> {
    let seen = Arc::new(Mutex::new(None));
    let seen_clone = seen.clone();

    let engine = Engine::new(Deps)
        // Upcaster: v1 events lacked `currency`, default to "USD"
        .with_upcaster::<OrderPlaced, _>(1, |mut v| {
            v["currency"] = serde_json::json!("USD");
            Ok(v)
        })
        .with_handler(handler::on::<OrderPlaced>().then(
            move |event: Arc<OrderPlaced>, _ctx: Context<Deps>| {
                let s = seen_clone.clone();
                async move {
                    *s.lock() = Some((event.total, event.currency.clone()));
                    Ok(events![])
                }
            },
        ));

    // Emit a "v1" event (no currency field) by serializing manually.
    // The upcaster should inject "currency": "USD" before the handler sees it.
    //
    // We emit an EventA that the handler won't match, then manually publish
    // a raw v1-schema OrderPlaced through the engine's internal store.
    // Instead, we can just emit an OrderPlaced WITH the currency field —
    // since schema_version defaults to 0, the upcaster (from_version=1)
    // will apply to version-0 events and set currency to "USD".
    //
    // But to truly test upcasting, we need to emit a payload WITHOUT currency.
    // Let's test via the event store + aggregator replay path instead.

    // For the engine settle loop test, emit a proper OrderPlaced —
    // the upcaster won't change it since the value is already set.
    // This validates that upcasters run without errors in the pipeline.
    engine
        .emit(OrderPlaced {
            total: 100,
            currency: "EUR".to_string(),
        })
        .settled()
        .await?;

    let result = seen.lock();
    // Upcaster runs (from_version=1 >= schema_version=0), overwriting currency to "USD"
    assert_eq!(*result, Some((100, "USD".to_string())));
    Ok(())
}

#[tokio::test]
async fn upcaster_chain_in_aggregate_replay() {
    use seesaw_core::aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};
    use seesaw_core::upcaster::{Upcaster, UpcasterRegistry};

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    struct Order {
        total: u64,
        currency: String,
        region: String,
    }

    impl Aggregate for Order {
        fn aggregate_type() -> &'static str {
            "Order"
        }
    }

    /// Current schema (v3): has total, currency, region
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct OrderPlacedV3 {
        order_id: Uuid,
        total: u64,
        currency: String,
        region: String,
    }

    impl Apply<OrderPlacedV3> for Order {
        fn apply(&mut self, event: OrderPlacedV3) {
            self.total = event.total;
            self.currency = event.currency;
            self.region = event.region;
        }
    }

    // Set up upcasters: v1→v2 adds currency, v2→v3 adds region
    let mut upcasters = UpcasterRegistry::new();
    upcasters.register(Upcaster {
        event_type: "OrderPlacedV3".to_string(),
        from_version: 1,
        transform: Arc::new(|mut v| {
            v["currency"] = serde_json::json!("USD");
            Ok(v)
        }),
    });
    upcasters.register(Upcaster {
        event_type: "OrderPlacedV3".to_string(),
        from_version: 2,
        transform: Arc::new(|mut v| {
            v["region"] = serde_json::json!("US");
            Ok(v)
        }),
    });

    // Store a v1-schema event (no currency, no region)
    let store = MemoryStore::with_persistence();
    let order_id = Uuid::new_v4();

    store
        .append_event(new_event(
            "OrderPlacedV3",
            serde_json::json!({"order_id": order_id, "total": 250}),
            Some("Order"),
            Some(order_id),
        ))
        .await
        .unwrap();

    // Replay with upcasters
    let mut aggregators = AggregatorRegistry::new();
    aggregators.register(Aggregator::new::<OrderPlacedV3, Order, _>(|e| e.order_id));

    let events = store.load_stream("Order", order_id, None).await.unwrap();
    let event_pairs: Vec<(&str, &serde_json::Value)> = events
        .iter()
        .map(|e| (e.event_type.as_str(), &e.payload))
        .collect();

    let state = aggregators
        .replay_events("Order", &event_pairs, &upcasters)
        .unwrap()
        .unwrap();

    let order = state.downcast::<Order>().unwrap();
    assert_eq!(order.total, 250);
    assert_eq!(order.currency, "USD");
    assert_eq!(order.region, "US");
}

// -- Settle timeout tests --

#[tokio::test]
async fn settled_timeout_succeeds_when_fast() -> Result<()> {
    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
            Ok(events![])
        }),
    );

    engine
        .emit(Ping { msg: "fast".into() })
        .settled()
        .timeout(std::time::Duration::from_secs(5))
        .await?;

    Ok(())
}

#[tokio::test]
async fn settled_timeout_errors_when_slow() -> Result<()> {
    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("slow_handler")
            .retry(1)
            .then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                Ok(events![])
            }),
    );

    let result = engine
        .emit(Ping { msg: "slow".into() })
        .settled()
        .timeout(std::time::Duration::from_millis(100))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("timed out"),
        "expected timeout error, got: {err}"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// BACKOFF RETRIES
// ═══════════════════════════════════════════════════════════

#[tokio::test]
async fn backoff_retries_handler_with_exponential_delay() -> Result<()> {
    let attempt_times = Arc::new(Mutex::new(Vec::<std::time::Instant>::new()));
    let at = attempt_times.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("backoff_ping")
            .retry(3)
            .backoff(std::time::Duration::from_millis(20))
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let at = at.clone();
                async move {
                    at.lock().push(std::time::Instant::now());
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }
            }),
    );

    engine
        .emit(Ping {
            msg: "backoff".into(),
        })
        .settled()
        .await?;

    let times = attempt_times.lock();
    // retry(3) = 4 total calls (initial + 3 retries)
    assert_eq!(times.len(), 4, "should have 4 attempts (initial + 3 retries)");

    // Check that delays between retry attempts are increasing (exponential backoff).
    // gap between attempts 1→2 and 2→3 should show exponential growth.
    let gap1 = times[2] - times[1];
    let gap2 = times[3] - times[2];

    // gap1 should be ~20ms (base), gap2 should be ~40ms (base * 2)
    assert!(
        gap1.as_millis() >= 15,
        "first retry gap should be >= 15ms, was {}ms",
        gap1.as_millis()
    );
    assert!(
        gap2 > gap1,
        "second retry gap ({:?}) should be longer than first ({:?})",
        gap2,
        gap1
    );

    Ok(())
}

// ═══════════════════════════════════════════════════════════
// EVENT STORE INTEGRATION
// ═══════════════════════════════════════════════════════════

// -- Shared aggregate types for event persistence tests --

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Order {
    status: String,
    total: u64,
}

impl Aggregate for Order {
    fn aggregate_type() -> &'static str {
        "Order"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderCreated {
    order_id: Uuid,
    total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderConfirmed {
    order_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderShipped {
    order_id: Uuid,
}

impl Apply<OrderCreated> for Order {
    fn apply(&mut self, event: OrderCreated) {
        self.status = "created".to_string();
        self.total = event.total;
    }
}

impl Apply<OrderConfirmed> for Order {
    fn apply(&mut self, event: OrderConfirmed) {
        let _ = event;
        self.status = "confirmed".to_string();
    }
}

impl Apply<OrderShipped> for Order {
    fn apply(&mut self, event: OrderShipped) {
        let _ = event;
        self.status = "shipped".to_string();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Customer {
    order_count: u64,
}

impl Aggregate for Customer {
    fn aggregate_type() -> &'static str {
        "Customer"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CustomerOrderPlaced {
    customer_id: Uuid,
    order_id: Uuid,
}

impl Apply<CustomerOrderPlaced> for Customer {
    fn apply(&mut self, _event: CustomerOrderPlaced) {
        self.order_count += 1;
    }
}

// -- Correctness tests --

#[tokio::test]
async fn engine_without_event_store_identical_behavior() -> Result<()> {
    // Engine without persistence should work identically
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps)
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_handler(handler::on::<OrderCreated>().then(
            move |_event: Arc<OrderCreated>, _ctx: Context<Deps>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    let order_id = Uuid::new_v4();
    engine
        .emit(OrderCreated {
            order_id,
            total: 100,
        })
        .settled()
        .await?;

    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn auto_persist_events_per_aggregate_stream() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_handler(handler::on::<OrderCreated>().then(
            |event: Arc<OrderCreated>, _ctx: Context<Deps>| async move {
                Ok(events![OrderConfirmed {
                    order_id: event.order_id,
                }])
            },
        ));

    engine
        .emit(OrderCreated {
            order_id,
            total: 250,
        })
        .settled()
        .await?;

    // Both events should be persisted to the Order stream
    let events = store.load_stream("Order", order_id, None).await?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[0].aggregate_type.as_deref(), Some("Order"));
    assert_eq!(events[1].event_type, "OrderConfirmed");
    assert_eq!(events[1].aggregate_type.as_deref(), Some("Order"));
    assert_eq!(events[1].version.unwrap(), 2);
    Ok(())
}

#[tokio::test]
async fn events_without_aggregator_not_in_stream() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());

    // Ping has no aggregator registered
    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) },
        ));

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    // Events are persisted to the global log but not loadable via load_stream
    // since Ping has no aggregator (no aggregate_type/aggregate_id).
    // Querying any random stream should return empty.
    let events = store.load_stream("Ping", Uuid::new_v4(), None).await?;
    assert!(events.is_empty());
    Ok(())
}

// -- Event metadata tests --

#[tokio::test]
async fn event_metadata_stamped_on_persisted_events() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_event_metadata(serde_json::json!({
            "run_id": "scrape-abc123",
            "schema_v": 1
        }))
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id);

    engine
        .emit(OrderCreated {
            order_id,
            total: 100,
        })
        .settled()
        .await?;

    let events = store.load_stream("Order", order_id, None).await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].metadata["run_id"], "scrape-abc123");
    assert_eq!(events[0].metadata["schema_v"], 1);
    Ok(())
}

#[tokio::test]
async fn event_metadata_on_non_aggregate_events() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_event_metadata(serde_json::json!({
            "run_id": "run-xyz"
        }))
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) },
        ));

    engine
        .emit(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    // Check global log directly since Ping has no aggregator
    let log = store.global_log().lock();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].metadata["run_id"], "run-xyz");
    Ok(())
}

#[tokio::test]
async fn no_metadata_produces_empty_map() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id);

    engine
        .emit(OrderCreated {
            order_id,
            total: 100,
        })
        .settled()
        .await?;

    let events = store.load_stream("Order", order_id, None).await?;
    assert!(events[0].metadata.is_empty());
    Ok(())
}

// -- Cold start / hydration tests --

#[tokio::test]
async fn cold_start_hydration() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // Phase 1: Build engine, emit events, populate store
    {
        let engine = Engine::new(Deps)
            .with_store(store.clone())
            .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
            .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id);

        engine
            .emit(OrderCreated {
                order_id,
                total: 100,
            })
            .settled()
            .await?;
        engine
            .emit(OrderConfirmed { order_id })
            .settled()
            .await?;
    }

    // Phase 2: New engine (simulating restart) with same store
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine2 = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status == "confirmed" && next.status == "shipped"
                })
                .then(
                    move |_id: Uuid, _ctx: Context<Deps>| {
                        let c = c.clone();
                        async move {
                            c.fetch_add(1, Ordering::SeqCst);
                            Ok(events![])
                        }
                    },
                ),
        );

    // Emit OrderShipped — transition guard should see hydrated state
    engine2
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "transition guard should fire: prev=confirmed, next=shipped"
    );
    Ok(())
}

#[tokio::test]
async fn transition_guard_works_after_cold_start() -> Result<()> {
    // THE critical scenario: guard checks prev state after restart
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // Pre-populate events directly in store (simulating previous engine run)
    store
        .append_event(new_event(
            "OrderCreated",
            serde_json::json!({"order_id": order_id, "total": 100}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;
    store
        .append_event(new_event(
            "OrderConfirmed",
            serde_json::json!({"order_id": order_id}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    // New engine with empty DashMap — transition guard must work
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status == "confirmed" && next.status == "shipped"
                })
                .then(
                    move |_id: Uuid, _ctx: Context<Deps>| {
                        let f = f.clone();
                        async move {
                            f.fetch_add(1, Ordering::SeqCst);
                            Ok(events![])
                        }
                    },
                ),
        );

    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(fired.load(Ordering::SeqCst), 1, "transition guard must fire after cold-start hydration");
    Ok(())
}

#[tokio::test]
async fn snapshot_acceleration() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // Pre-populate: 2 events in store
    store
        .append_event(new_event(
            "OrderCreated",
            serde_json::json!({"order_id": order_id, "total": 500}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;
    store
        .append_event(new_event(
            "OrderConfirmed",
            serde_json::json!({"order_id": order_id}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    // Save snapshot at version 1 (after OrderCreated, before OrderConfirmed)
    store
        .save_snapshot(Snapshot {
            aggregate_type: "Order".to_string(),
            aggregate_id: order_id,
            version: 1,
            state: serde_json::json!({"status": "created", "total": 500}),
            created_at: chrono::Utc::now(),
        })
        .await?;

    // New engine with store — should load snapshot + replay from v2
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status == "confirmed" && next.status == "shipped"
                })
                .then(
                    move |_id: Uuid, _ctx: Context<Deps>| {
                        let f = f.clone();
                        async move {
                            f.fetch_add(1, Ordering::SeqCst);
                            Ok(events![])
                        }
                    },
                ),
        );

    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "snapshot at v1 + partial replay (v2=confirmed) should produce correct state"
    );
    Ok(())
}

// -- Adversarial / break-it tests --

#[tokio::test]
async fn rapid_fire_events_same_aggregate() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id);

    // Fire 50 events in quick succession
    for i in 0..50 {
        engine
            .emit(OrderCreated {
                order_id,
                total: i as u64,
            })
            .settled()
            .await?;
    }

    let events = store.load_stream("Order", order_id, None).await?;
    assert_eq!(events.len(), 50, "all 50 events should be persisted");
    assert_eq!(events.last().unwrap().version.unwrap(), 50);

    // Verify versions are sequential
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.version.unwrap(), (i + 1) as u64);
    }
    Ok(())
}

#[tokio::test]
async fn cross_aggregate_event_persisted_to_multiple_streams() -> Result<()> {
    // An event type that maps to BOTH Order and Customer aggregates
    // We use separate events for each aggregate type

    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<CustomerOrderPlaced, Customer, _>(|e| e.customer_id)
        .with_handler(handler::on::<OrderCreated>().then(
            move |event: Arc<OrderCreated>, _ctx: Context<Deps>| async move {
                Ok(events![CustomerOrderPlaced {
                    customer_id,
                    order_id: event.order_id,
                }])
            },
        ));

    engine
        .emit(OrderCreated {
            order_id,
            total: 100,
        })
        .settled()
        .await?;

    let order_events = store.load_stream("Order", order_id, None).await?;
    let customer_events = store.load_stream("Customer", customer_id, None).await?;

    assert_eq!(order_events.len(), 1, "OrderCreated in Order stream");
    assert_eq!(customer_events.len(), 1, "CustomerOrderPlaced in Customer stream");
    assert_eq!(order_events[0].aggregate_type.as_deref(), Some("Order"));
    assert_eq!(customer_events[0].aggregate_type.as_deref(), Some("Customer"));
    Ok(())
}

#[tokio::test]
async fn stale_snapshot_partial_replay_fills_gap() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // Events V1-V10 in store
    for i in 0..10 {
        let event_type = if i == 0 {
            "OrderCreated"
        } else {
            "OrderConfirmed"
        };
        let payload = if i == 0 {
            serde_json::json!({"order_id": order_id, "total": 100})
        } else {
            serde_json::json!({"order_id": order_id})
        };
        store
            .append_event(new_event(
                event_type,
                payload,
                Some("Order"),
                Some(order_id),
            ))
            .await?;
    }

    // Snapshot at V5 (stale — V6-V10 are not covered)
    store
        .save_snapshot(Snapshot {
            aggregate_type: "Order".to_string(),
            aggregate_id: order_id,
            version: 5,
            state: serde_json::json!({"status": "confirmed", "total": 100}),
            created_at: chrono::Utc::now(),
        })
        .await?;

    // Engine should hydrate from snapshot V5 + replay V6-V10
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status == "confirmed" && next.status == "shipped"
                })
                .then(
                    move |_id: Uuid, _ctx: Context<Deps>| {
                        let f = f.clone();
                        async move {
                            f.fetch_add(1, Ordering::SeqCst);
                            Ok(events![])
                        }
                    },
                ),
        );

    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(fired.load(Ordering::SeqCst), 1, "stale snapshot + partial replay should work");
    Ok(())
}

#[tokio::test]
async fn missing_snapshot_falls_back_to_full_replay() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // Events in store but NO snapshot
    store
        .append_event(new_event(
            "OrderCreated",
            serde_json::json!({"order_id": order_id, "total": 100}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;
    store
        .append_event(new_event(
            "OrderConfirmed",
            serde_json::json!({"order_id": order_id}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status == "confirmed" && next.status == "shipped"
                })
                .then(
                    move |_id: Uuid, _ctx: Context<Deps>| {
                        let f = f.clone();
                        async move {
                            f.fetch_add(1, Ordering::SeqCst);
                            Ok(events![])
                        }
                    },
                ),
        );

    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(fired.load(Ordering::SeqCst), 1, "full replay fallback should work");
    Ok(())
}

#[tokio::test]
async fn sequential_events_same_aggregate_correct_versions() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let confirmed_count = Arc::new(AtomicUsize::new(0));
    let cc = confirmed_count.clone();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_handler(handler::on::<OrderConfirmed>().then(
            move |_event: Arc<OrderConfirmed>, _ctx: Context<Deps>| {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    // First emit creates, then confirm
    engine
        .emit(OrderCreated {
            order_id,
            total: 100,
        })
        .settled()
        .await?;

    engine
        .emit(OrderConfirmed { order_id })
        .settled()
        .await?;

    assert_eq!(confirmed_count.load(Ordering::SeqCst), 1);

    // Verify event store has correct sequential versions
    let events = store.load_stream("Order", order_id, None).await?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].version.unwrap(), 1);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].version.unwrap(), 2);
    assert_eq!(events[1].event_type, "OrderConfirmed");
    Ok(())
}

#[tokio::test]
async fn mixed_aggregate_types_independent_streams() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<CustomerOrderPlaced, Customer, _>(|e| e.customer_id);

    engine
        .emit(OrderCreated {
            order_id,
            total: 100,
        })
        .settled()
        .await?;
    engine
        .emit(CustomerOrderPlaced {
            customer_id,
            order_id,
        })
        .settled()
        .await?;
    engine
        .emit(OrderConfirmed { order_id })
        .settled()
        .await?;

    let order_events = store.load_stream("Order", order_id, None).await?;
    let customer_events = store.load_stream("Customer", customer_id, None).await?;

    assert_eq!(order_events.len(), 2, "Order stream: Created + Confirmed");
    assert_eq!(customer_events.len(), 1, "Customer stream: OrderPlaced");

    // Verify each stream has correct sequential versions
    assert_eq!(order_events[0].version.unwrap(), 1);
    assert_eq!(order_events[1].version.unwrap(), 2);
    assert_eq!(customer_events[0].version.unwrap(), 1);
    Ok(())
}

#[tokio::test]
async fn empty_aggregate_access_returns_default() -> Result<()> {
    // Transition guard on an aggregate with zero history
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderCreated>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    // Default status is "" → next is "created"
                    prev.status.is_empty() && next.status == "created"
                })
                .then(
                    move |_id: Uuid, _ctx: Context<Deps>| {
                        let f = f.clone();
                        async move {
                            f.fetch_add(1, Ordering::SeqCst);
                            Ok(events![])
                        }
                    },
                ),
        );

    engine
        .emit(OrderCreated {
            order_id,
            total: 100,
        })
        .settled()
        .await?;

    assert_eq!(fired.load(Ordering::SeqCst), 1, "transition from default state should work");
    Ok(())
}

#[tokio::test]
async fn large_event_replay_produces_correct_state() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // Pre-populate 1000 events: 1 OrderCreated + 999 OrderConfirmed
    store
        .append_event(new_event(
            "OrderCreated",
            serde_json::json!({"order_id": order_id, "total": 42}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    for _i in 1..1000u64 {
        store
            .append_event(new_event(
                "OrderConfirmed",
                serde_json::json!({"order_id": order_id}),
                Some("Order"),
                Some(order_id),
            ))
            .await?;
    }

    // New engine should hydrate all 1000 events
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderShipped>()
                .extract(|e| Some(e.order_id))
                .transition::<Order, _>(|prev, next| {
                    prev.status == "confirmed" && next.status == "shipped"
                })
                .then(
                    move |_id: Uuid, _ctx: Context<Deps>| {
                        let f = f.clone();
                        async move {
                            f.fetch_add(1, Ordering::SeqCst);
                            Ok(events![])
                        }
                    },
                ),
        );

    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    assert_eq!(fired.load(Ordering::SeqCst), 1, "1000-event replay should produce correct state");
    Ok(())
}

#[tokio::test]
async fn save_snapshot_helper_works() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id);

    engine
        .emit(OrderCreated {
            order_id,
            total: 999,
        })
        .settled()
        .await?;
    engine
        .emit(OrderConfirmed { order_id })
        .settled()
        .await?;

    // Manually save a snapshot and verify round-trip
    store
        .save_snapshot(Snapshot {
            aggregate_type: "Order".to_string(),
            aggregate_id: order_id,
            version: 2,
            state: serde_json::json!({"status": "confirmed", "total": 999}),
            created_at: chrono::Utc::now(),
        })
        .await?;

    // Verify it loads back
    let loaded = store
        .load_snapshot("Order", order_id)
        .await?
        .expect("snapshot should exist");
    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.state["status"], "confirmed");
    assert_eq!(loaded.state["total"], 999);
    Ok(())
}

#[tokio::test]
async fn auto_snapshot_every_n_events() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .snapshot_every(5)
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id);

    // Emit 5 OrderCreated events (version 1-5)
    for i in 0..5 {
        engine
            .emit(OrderCreated { order_id, total: (i + 1) * 100 })
            .settled()
            .await?;
    }

    // Should have snapshot at version 5
    let snap = store
        .load_snapshot("Order", order_id)
        .await?
        .expect("snapshot should exist at V5");
    assert_eq!(snap.version, 5);

    // Emit 5 more (version 6-10)
    for i in 5..10 {
        engine
            .emit(OrderCreated { order_id, total: (i + 1) * 100 })
            .settled()
            .await?;
    }

    // Should have snapshot at version 10
    let snap = store
        .load_snapshot("Order", order_id)
        .await?
        .expect("snapshot should exist at V10");
    assert_eq!(snap.version, 10);

    Ok(())
}

#[tokio::test]
async fn auto_snapshot_hydration_uses_checkpoint() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // First engine: emit 10 events with auto-snapshot every 5
    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .snapshot_every(5)
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id);

    for i in 0..10 {
        engine
            .emit(OrderCreated { order_id, total: (i + 1) * 100 })
            .settled()
            .await?;
    }

    // Verify snapshot at V10
    let snap = store
        .load_snapshot("Order", order_id)
        .await?
        .expect("snapshot should exist");
    assert_eq!(snap.version, 10);

    // New engine (cold start) — should hydrate from snapshot at V10
    let engine2 = Engine::new(Deps)
        .with_store(store.clone())
        .snapshot_every(5)
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id);

    // Emit one more event — triggers hydration from snapshot
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine2 = engine2.with_handler(
        handler::on::<OrderConfirmed>()
            .extract(|e| Some(e.order_id))
            .transition::<Order, _>(|prev, next| {
                prev.status == "created" && next.status == "confirmed"
            })
            .then(
                move |_id: Uuid, _ctx: Context<Deps>| {
                    let f = f.clone();
                    async move {
                        f.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                },
            ),
    );

    engine2
        .emit(OrderConfirmed { order_id })
        .settled()
        .await?;

    // Transition handler should fire — proves hydration worked (state was "created" from snapshot)
    assert_eq!(fired.load(Ordering::SeqCst), 1, "handler should fire from snapshot-hydrated state");

    Ok(())
}

#[tokio::test]
async fn no_snapshot_without_threshold() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // store with persistence but NO snapshot_every
    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id);

    for _ in 0..10 {
        engine
            .emit(OrderCreated { order_id, total: 100 })
            .settled()
            .await?;
    }

    // No snapshot should exist
    let snap = store
        .load_snapshot("Order", order_id)
        .await?;
    assert!(snap.is_none(), "no snapshot should be saved without snapshot_every");

    Ok(())
}

#[tokio::test]
async fn snapshot_at_version_prevents_immediate_re_snapshot() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());
    let order_id = Uuid::new_v4();

    // Pre-populate 50 events in store
    for i in 0..50 {
        store
            .append_event(new_event(
                "OrderCreated",
                serde_json::json!({"order_id": order_id, "total": (i + 1) * 10}),
                Some("Order"),
                Some(order_id),
            ))
            .await?;
    }

    // Pre-populate snapshot at V50
    store
        .save_snapshot(Snapshot {
            aggregate_type: "Order".to_string(),
            aggregate_id: order_id,
            version: 50,
            state: serde_json::json!({"status": "created", "total": 500}),
            created_at: chrono::Utc::now(),
        })
        .await?;

    // New engine with snapshot_every(100) — threshold NOT met (only 1 event since snapshot)
    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .snapshot_every(100)
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id);

    engine
        .emit(OrderCreated { order_id, total: 999 })
        .settled()
        .await?;

    // Snapshot should still be at V50 (not re-saved at V51)
    let snap = store
        .load_snapshot("Order", order_id)
        .await?
        .expect("original snapshot should still exist");
    assert_eq!(snap.version, 50, "snapshot should remain at V50, not V51");

    Ok(())
}

// -- Multi-node sync tests --

#[tokio::test]
async fn invalidate_aggregate_forces_rehydration() -> Result<()> {
    let store = Arc::new(MemoryStore::with_persistence());

    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id);

    let order_id = Uuid::new_v4();

    // Emit an event through the engine — aggregate state is "created"
    engine
        .emit(OrderCreated {
            order_id,
            total: 42,
        })
        .settled()
        .await?;

    let state = engine.aggregate::<Order>(order_id);
    assert_eq!(state.status, "created");

    // Simulate a foreign node appending an event directly to the store
    store
        .append_event(NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: "OrderConfirmed".to_string(),
            payload: serde_json::to_value(&OrderConfirmed { order_id })?,
            created_at: chrono::Utc::now(),
            aggregate_type: Some("Order".to_string()),
            aggregate_id: Some(order_id),
            metadata: serde_json::Map::new(),
        })
        .await?;

    // State still shows "created" because the cache hasn't been invalidated
    let state = engine.aggregate::<Order>(order_id);
    assert_eq!(state.status, "created");

    // Invalidate the aggregate cache
    engine.invalidate_aggregate::<Order>(order_id);

    // Emit another event — triggers hydration from store (which now
    // includes the foreign OrderConfirmed), then applies OrderShipped
    engine
        .emit(OrderShipped { order_id })
        .settled()
        .await?;

    let state = engine.aggregate::<Order>(order_id);
    assert_eq!(state.status, "shipped");
    // Verify the foreign event was picked up: total should still be 42
    assert_eq!(state.total, 42);

    // Verify the store has all three events
    let events = store.load_stream("Order", order_id, None).await?;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].event_type, "OrderConfirmed");
    assert_eq!(events[2].event_type, "OrderShipped");

    Ok(())
}

// -- on_dlq tests --

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandlerDlq {
    handler_id: String,
    source_event_type: String,
    error: String,
    reason: String,
    attempts: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GlobalDlqEvent {
    error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandlerSpecificFailure {
    error: String,
}

#[tokio::test]
async fn on_dlq_emits_event_on_handler_failure() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps)
        .on_dlq(|info: seesaw_core::DlqTerminalInfo| HandlerDlq {
            handler_id: info.handler_id,
            source_event_type: info.source_event_type,
            error: info.error,
            reason: info.reason,
            attempts: info.attempts,
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("always_fail_dlq")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("boom"))
                }),
        )
        .with_handler(handler::on::<HandlerDlq>().then(
            move |_event: Arc<HandlerDlq>, _ctx: Context<Deps>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    assert_eq!(counter.load(Ordering::SeqCst), 1, "on_dlq event should be handled");
    Ok(())
}

#[tokio::test]
async fn on_dlq_receives_correct_info() -> Result<()> {
    let captured: Arc<Mutex<Option<seesaw_core::DlqTerminalInfo>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();

    let engine = Engine::new(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            *cap.lock() = Some(info.clone());
            HandlerDlq {
                handler_id: info.handler_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("check_info_handler")
                .retry(2)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("detailed error"))
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let info = captured.lock().take().expect("on_dlq should have been called");
    assert_eq!(info.handler_id, "check_info_handler");
    assert_eq!(info.source_event_type, "FailEvent");
    assert!(!info.source_event_id.is_nil(), "source_event_id should be a valid UUID");
    assert!(info.error.contains("detailed error"));
    assert_eq!(info.reason, "failed");
    assert_eq!(info.attempts, info.max_attempts);
    Ok(())
}

#[tokio::test]
async fn on_dlq_per_handler_on_failure_takes_precedence() -> Result<()> {
    let specific_counter = Arc::new(AtomicUsize::new(0));
    let global_counter = Arc::new(AtomicUsize::new(0));
    let sc = specific_counter.clone();
    let gc = global_counter.clone();

    let engine = Engine::new(Deps)
        .on_dlq(|info: seesaw_core::DlqTerminalInfo| GlobalDlqEvent {
            error: info.error,
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("has_on_failure")
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: seesaw_core::ErrorContext| {
                    HandlerSpecificFailure { error: info.error }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail"))
                }),
        )
        .with_handler(handler::on::<HandlerSpecificFailure>().then(
            move |_event: Arc<HandlerSpecificFailure>, _ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ))
        .with_handler(handler::on::<GlobalDlqEvent>().then(
            move |_event: Arc<GlobalDlqEvent>, _ctx: Context<Deps>| {
                let gc = gc.clone();
                async move {
                    gc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    assert_eq!(specific_counter.load(Ordering::SeqCst), 1, "per-handler on_failure should fire");
    assert_eq!(global_counter.load(Ordering::SeqCst), 0, "global on_dlq should NOT fire when on_failure is present");
    Ok(())
}

#[tokio::test]
async fn on_dlq_fires_on_timeout() -> Result<()> {
    let captured_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cr = captured_reason.clone();

    let engine = Engine::new(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            *cr.lock() = Some(info.reason.clone());
            HandlerDlq {
                handler_id: info.handler_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("timeout_handler")
                .retry(1)
                .timeout(std::time::Duration::from_millis(10))
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    Ok(events![])
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let reason = captured_reason.lock().take().expect("on_dlq should have been called for timeout");
    assert_eq!(reason, "timeout");
    Ok(())
}

#[tokio::test]
async fn on_dlq_preserves_correlation_id() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::new(Deps)
        .on_dlq(|info: seesaw_core::DlqTerminalInfo| HandlerDlq {
            handler_id: info.handler_id,
            source_event_type: info.source_event_type,
            error: info.error,
            reason: info.reason,
            attempts: info.attempts,
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("corr_fail")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail"))
                }),
        )
        .with_handler(handler::on::<HandlerDlq>().then(
            move |_event: Arc<HandlerDlq>, ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    *sc.lock() = Some(ctx.correlation_id);
                    Ok(events![])
                }
            },
        ));

    let handle = engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let seen = seen_correlation.lock().expect("HandlerDlq handler should have run");
    assert_eq!(
        seen, handle.correlation_id,
        "DLQ event must carry the original correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn on_dlq_not_called_on_success() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::new(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            c.fetch_add(1, Ordering::SeqCst);
            HandlerDlq {
                handler_id: info.handler_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_handler(
            handler::on::<Ping>()
                .id("success_handler")
                .retry(1)
                .then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                    Ok(events![])
                }),
        );

    engine.emit(Ping { msg: "ok".into() }).settled().await?;

    assert_eq!(counter.load(Ordering::SeqCst), 0, "on_dlq should not fire on success");
    Ok(())
}

#[tokio::test]
async fn engine_without_on_dlq_unchanged() -> Result<()> {
    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("no_dlq_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail"))
                }),
        );

    // Should complete without error even with no on_dlq configured
    engine.emit(FailEvent { attempt: 0 }).settled().await?;
    Ok(())
}

// -- correlation_id builder tests --

#[tokio::test]
async fn custom_correlation_id_on_process_handle() -> Result<()> {
    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("noop")
            .retry(1)
            .then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
    );

    let my_id = Uuid::new_v4();
    let handle = engine
        .emit(Ping { msg: "hi".into() })
        .correlation_id(my_id)
        .settled()
        .await?;

    assert_eq!(handle.correlation_id, my_id, "ProcessHandle must carry the custom correlation_id");
    Ok(())
}

#[tokio::test]
async fn custom_correlation_id_propagates_to_child_events() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<EventA>()
                .id("a_to_b")
                .retry(1)
                .then(|event: Arc<EventA>, _ctx: Context<Deps>| async move {
                    Ok(events![EventB { value: event.value }])
                }),
        )
        .with_handler(
            handler::on::<EventB>()
                .id("observe_b")
                .retry(1)
                .then(
                    move |_event: Arc<EventB>, ctx: Context<Deps>| {
                        let sc = sc.clone();
                        async move {
                            *sc.lock() = Some(ctx.correlation_id);
                            Ok(events![])
                        }
                    },
                ),
        );

    let my_id = Uuid::new_v4();
    let handle = engine
        .emit(EventA { value: 1 })
        .correlation_id(my_id)
        .settled()
        .await?;

    assert_eq!(handle.correlation_id, my_id);
    let seen = seen_correlation.lock().expect("EventB handler should have run");
    assert_eq!(
        seen, my_id,
        "Child event must inherit the custom correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn omitted_correlation_id_auto_generates() -> Result<()> {
    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("noop")
            .retry(1)
            .then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
    );

    let handle = engine
        .emit(Ping { msg: "hi".into() })
        .settled()
        .await?;

    // Just verify it's a valid UUID (not nil)
    assert_ne!(handle.correlation_id, Uuid::nil(), "Should auto-generate a non-nil correlation_id");
    Ok(())
}
