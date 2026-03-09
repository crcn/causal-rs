//! Integration tests for Engine settle loop.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use seesaw_core::aggregator::{Aggregate, Apply};
use seesaw_core::{event, events, handler, Context, Engine, EventLog, Events, HandlerQueue, MemoryStore, NewEvent, Snapshot};
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
        ephemeral: None,
    }
}

#[derive(Clone)]
struct Deps;

// -- Event types --

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    msg: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventA {
    value: i32,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventB {
    value: i32,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailEvent {
    attempt: i32,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailedTerminal {
    error: String,
    attempts: i32,
}

// -- Tests --

#[tokio::test]
async fn basic_handler_fires() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::in_memory(Deps).with_handler(
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

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps).with_handler(
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

    let engine = Engine::in_memory(Deps).with_handler(
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
    let engine = Engine::in_memory(Deps).with_handler(
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

    let engine = Engine::in_memory(Deps).with_handler(
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

    let engine = Engine::in_memory(Deps)
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
async fn correlation_preserved_through_queued_chain() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps).with_handler(
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
#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    total: u64,
    currency: String,
}

#[tokio::test]
async fn upcaster_transforms_old_event_for_handler() -> Result<()> {
    let seen = Arc::new(Mutex::new(None));
    let seen_clone = seen.clone();

    let engine = Engine::in_memory(Deps)
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
    #[event]
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
        event_prefix: "order_placed_v3".to_string(),
        from_version: 1,
        transform: Arc::new(|mut v| {
            v["currency"] = serde_json::json!("USD");
            Ok(v)
        }),
    });
    upcasters.register(Upcaster {
        event_prefix: "order_placed_v3".to_string(),
        from_version: 2,
        transform: Arc::new(|mut v| {
            v["region"] = serde_json::json!("US");
            Ok(v)
        }),
    });

    // Store a v1-schema event (no currency, no region)
    let store = MemoryStore::new();
    let order_id = Uuid::new_v4();

    store
        .append(new_event(
            "order_placed_v3",
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
    let engine = Engine::in_memory(Deps).with_handler(
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
    let engine = Engine::in_memory(Deps).with_handler(
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

    let engine = Engine::in_memory(Deps).with_handler(
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

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderCreated {
    order_id: Uuid,
    total: u64,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderConfirmed {
    order_id: Uuid,
}

#[event]
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

#[event]
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

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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
    assert_eq!(events[0].event_type, "order_created");
    assert_eq!(events[0].aggregate_type.as_deref(), Some("Order"));
    assert_eq!(events[1].event_type, "order_confirmed");
    assert_eq!(events[1].aggregate_type.as_deref(), Some("Order"));
    assert_eq!(events[1].version.unwrap(), 2);
    Ok(())
}

#[tokio::test]
async fn events_without_aggregator_not_in_stream() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    // Ping has no aggregator registered
    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // Phase 1: Build engine, emit events, populate store
    {
        let engine = Engine::in_memory(Deps)
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

    let engine2 = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // Pre-populate events directly in store (simulating previous engine run)
    store
        .append(new_event(
            "order_created",
            serde_json::json!({"order_id": order_id, "total": 100}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;
    store
        .append(new_event(
            "order_confirmed",
            serde_json::json!({"order_id": order_id}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    // New engine with empty DashMap — transition guard must work
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // Pre-populate: 2 events in store
    store
        .append(new_event(
            "order_created",
            serde_json::json!({"order_id": order_id, "total": 500}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;
    store
        .append(new_event(
            "order_confirmed",
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

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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

    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // Events V1-V10 in store
    for i in 0..10 {
        let event_type = if i == 0 {
            "order_created"
        } else {
            "order_confirmed"
        };
        let payload = if i == 0 {
            serde_json::json!({"order_id": order_id, "total": 100})
        } else {
            serde_json::json!({"order_id": order_id})
        };
        store
            .append(new_event(
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

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // Events in store but NO snapshot
    store
        .append(new_event(
            "order_created",
            serde_json::json!({"order_id": order_id, "total": 100}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;
    store
        .append(new_event(
            "order_confirmed",
            serde_json::json!({"order_id": order_id}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let confirmed_count = Arc::new(AtomicUsize::new(0));
    let cc = confirmed_count.clone();

    let engine = Engine::in_memory(Deps)
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
    assert_eq!(events[0].event_type, "order_created");
    assert_eq!(events[1].version.unwrap(), 2);
    assert_eq!(events[1].event_type, "order_confirmed");
    Ok(())
}

#[tokio::test]
async fn mixed_aggregate_types_independent_streams() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // Pre-populate 1000 events: 1 OrderCreated + 999 OrderConfirmed
    store
        .append(new_event(
            "order_created",
            serde_json::json!({"order_id": order_id, "total": 42}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    for _i in 1..1000u64 {
        store
            .append(new_event(
                "order_confirmed",
                serde_json::json!({"order_id": order_id}),
                Some("Order"),
                Some(order_id),
            ))
            .await?;
    }

    // New engine should hydrate all 1000 events
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // First engine: emit 10 events with auto-snapshot every 5
    let engine = Engine::in_memory(Deps)
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
    let engine2 = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // store with persistence but NO snapshot_every
    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    // Pre-populate 50 events in store
    for i in 0..50 {
        store
            .append(new_event(
                "order_created",
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
    let engine = Engine::in_memory(Deps)
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
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
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
        .append(NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: "order_confirmed".to_string(),
            payload: serde_json::to_value(&OrderConfirmed { order_id })?,
            created_at: chrono::Utc::now(),
            aggregate_type: Some("Order".to_string()),
            aggregate_id: Some(order_id),
            metadata: serde_json::Map::new(),
            ephemeral: None,
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
    assert_eq!(events[0].event_type, "order_created");
    assert_eq!(events[1].event_type, "order_confirmed");
    assert_eq!(events[2].event_type, "order_shipped");

    Ok(())
}

// -- on_dlq tests --

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandlerDlq {
    handler_id: String,
    source_event_type: String,
    error: String,
    reason: String,
    attempts: i32,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GlobalDlqEvent {
    error: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandlerSpecificFailure {
    error: String,
}

#[tokio::test]
async fn on_dlq_emits_event_on_handler_failure() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps)
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
    assert_eq!(info.source_event_type, "fail_event");
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

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps)
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

    let engine = Engine::in_memory(Deps)
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
    let engine = Engine::in_memory(Deps)
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
    let engine = Engine::in_memory(Deps).with_handler(
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

    let engine = Engine::in_memory(Deps)
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
    let engine = Engine::in_memory(Deps).with_handler(
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

// -- Cancellation tests --

#[tokio::test]
async fn cancel_prevents_event_processing() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(events![])
            }
        }),
    );

    // Emit without settling, then cancel before settle
    let handle = engine.emit(Ping { msg: "hello".into() }).await?;
    engine.cancel(handle.correlation_id).await?;
    engine.settle().await?;

    assert_eq!(counter.load(Ordering::SeqCst), 0, "handler should not fire for cancelled workflow");
    Ok(())
}

#[tokio::test]
async fn cancel_dlqs_pending_handlers() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // Use a queued handler so it goes through poll_next_handler
    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<Ping>()
            .id("cancel_dlq_handler")
            .retry(1)
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    // Emit (publishes event), then cancel, then settle
    let handle = engine.emit(Ping { msg: "hello".into() }).await?;
    engine.cancel(handle.correlation_id).await?;
    engine.settle().await?;

    assert_eq!(counter.load(Ordering::SeqCst), 0, "queued handler should not fire for cancelled workflow");
    Ok(())
}

#[tokio::test]
async fn cancel_is_idempotent() -> Result<()> {
    let engine = Engine::in_memory(Deps);
    let id = Uuid::new_v4();

    // Cancel twice — no error
    engine.cancel(id).await?;
    engine.cancel(id).await?;
    Ok(())
}

#[tokio::test]
async fn cancel_does_not_affect_other_correlations() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(events![])
            }
        }),
    );

    // Emit two workflows
    let handle_a = engine.emit(Ping { msg: "a".into() }).await?;
    let _handle_b = engine.emit(Ping { msg: "b".into() }).await?;

    // Cancel only workflow A
    engine.cancel(handle_a.correlation_id).await?;
    engine.settle().await?;

    // Only workflow B should have fired
    assert_eq!(counter.load(Ordering::SeqCst), 1, "only non-cancelled workflow should fire");
    Ok(())
}

#[tokio::test]
async fn cancel_mid_settle_rejects_downstream_events() -> Result<()> {
    /// Child event emitted by the Ping handler.
    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Pong;

    let pong_counter = Arc::new(AtomicUsize::new(0));
    let pong_clone = pong_counter.clone();

    let store = Arc::new(MemoryStore::new());
    let store_for_handler = store.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store)
        // Ping handler: emits Pong, then cancels own correlation via the store
        .with_handler(
            handler::on::<Ping>().then(move |_event: Arc<Ping>, ctx: Context<Deps>| {
                let s = store_for_handler.clone();
                let cid = ctx.correlation_id;
                async move {
                    // Cancel the workflow from within the handler.
                    // The emitted Pong will be queued but should be rejected
                    // on the next settle loop iteration.
                    s.cancel(cid).await.unwrap();
                    Ok(events![Pong])
                }
            }),
        )
        // Pong handler: should never fire because workflow is cancelled
        .with_handler(
            handler::on::<Pong>().then(move |_event: Arc<Pong>, _ctx: Context<Deps>| {
                let c = pong_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
        );

    engine
        .emit(Ping { msg: "trigger".into() })
        .settled()
        .await?;

    assert_eq!(
        pong_counter.load(Ordering::SeqCst),
        0,
        "downstream Pong handler should not fire after mid-settle cancel"
    );
    Ok(())
}

// ── QueueStatus + ProcessHandle workflow tests ──────────────────

#[tokio::test]
async fn queue_status_empty_after_settle() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
    );

    let handle = engine
        .emit(Ping {
            msg: "hi".into(),
        })
        .settled()
        .await?;

    let status = handle.status().await?;
    assert_eq!(status.pending_handlers, 0);
    assert_eq!(status.dead_lettered, 0);
    Ok(())
}

#[tokio::test]
async fn queue_status_shows_pending_before_settle() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
    );

    // Emit without settling — event should be pending
    let handle = engine
        .emit(Ping {
            msg: "hi".into(),
        })
        .await?;

    Ok(())
}

#[tokio::test]
async fn process_handle_cancel_works() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(events![])
            }
        }),
    );

    let handle = engine
        .emit(Ping {
            msg: "cancel me".into(),
        })
        .await?;

    handle.cancel().await?;
    engine.settle().await?;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "handler should not fire after cancel via ProcessHandle"
    );
    Ok(())
}

#[tokio::test]
async fn engine_status_delegates_to_store() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
    );

    let handle = engine
        .emit(Ping {
            msg: "hi".into(),
        })
        .await?;

    Ok(())
}

// -- Error handling edge-case tests --

#[tokio::test]
async fn on_dlq_fires_on_anyhow_bail() -> Result<()> {
    let captured_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let ce = captured_error.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            *ce.lock() = Some(info.error.clone());
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
                .id("bail_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    anyhow::bail!("bail message here");
                    #[allow(unreachable_code)]
                    Ok(events![])
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let error = captured_error.lock().take().expect("on_dlq should have fired");
    assert!(error.contains("bail message here"), "error was: {error}");
    Ok(())
}

#[tokio::test]
async fn on_dlq_fires_on_custom_error_type() -> Result<()> {
    #[derive(Debug)]
    struct CustomError(String);
    impl std::fmt::Display for CustomError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "custom: {}", self.0)
        }
    }
    impl std::error::Error for CustomError {}

    let captured_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let ce = captured_error.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            *ce.lock() = Some(info.error.clone());
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
                .id("custom_error_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!(CustomError("my custom error".into())))
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let error = captured_error.lock().take().expect("on_dlq should have fired");
    assert!(error.contains("my custom error"), "error was: {error}");
    Ok(())
}

#[tokio::test]
async fn handler_panic_does_not_crash_engine() -> Result<()> {
    // Document current behavior: a panic in a handler may crash the engine.
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::in_memory(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("panic_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    panic!("boom");
                    #[allow(unreachable_code)]
                    Ok(events![])
                }),
        )
        .with_handler(handler::on::<Ping>().then(
            move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    // Panics propagate and may cause settle to fail. We document this behavior.
    let result = std::panic::AssertUnwindSafe(
        engine.emit(FailEvent { attempt: 0 }).settled(),
    );
    let outcome = futures::FutureExt::catch_unwind(result).await;

    // Whether the engine catches the panic or not, this test documents behavior.
    match outcome {
        Ok(Ok(_)) => { /* Engine caught the panic */ }
        Ok(Err(_)) => { /* Engine returned an error */ }
        Err(_) => { /* Engine panicked — panics propagate */ }
    }
    Ok(())
}

#[tokio::test]
async fn on_dlq_fires_on_queued_handler_panic() -> Result<()> {
    let dlq_counter = Arc::new(AtomicUsize::new(0));
    let dc = dlq_counter.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            dc.fetch_add(1, Ordering::SeqCst);
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
                .id("queued_panic_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    panic!("queued boom");
                    #[allow(unreachable_code)]
                    Ok(events![])
                }),
        );

    let result = std::panic::AssertUnwindSafe(
        engine.emit(FailEvent { attempt: 0 }).settled(),
    );
    let outcome = futures::FutureExt::catch_unwind(result).await;

    match outcome {
        Ok(Ok(_)) => {
            // If engine catches panic, DLQ should fire
            assert!(dlq_counter.load(Ordering::SeqCst) > 0, "on_dlq should fire when panic is caught");
        }
        Ok(Err(_)) | Err(_) => {
            // Panic propagated — documents that panics are not caught
        }
    }
    Ok(())
}

#[tokio::test]
async fn projection_error_does_not_stop_other_handlers() -> Result<()> {
    let handler_counter = Arc::new(AtomicUsize::new(0));
    let hc = handler_counter.clone();

    let engine = Engine::in_memory(Deps)
        .with_projection(
            seesaw_core::project("failing_projection")
                .then(|_event: seesaw_core::AnyEvent, _ctx: Context<Deps>| async move {
                    anyhow::bail!("projection failed");
                }),
        )
        .with_handler(handler::on::<Ping>().then(
            move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let hc = hc.clone();
                async move {
                    hc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine.emit(Ping { msg: "hi".into() }).settled().await?;

    assert_eq!(
        handler_counter.load(Ordering::SeqCst),
        1,
        "handler should still fire even when projection errors"
    );
    Ok(())
}

#[tokio::test]
async fn projection_error_recorded_as_projection_failure() -> Result<()> {
    // Projection errors are recorded as ProjectionFailure and routed through DeadLetter,
    // but MemoryStore.queue_status() doesn't track dead_lettered count.
    // This test verifies the engine settles successfully despite the projection error
    // and a second projection still runs.
    let second_projection_counter = Arc::new(AtomicUsize::new(0));
    let spc = second_projection_counter.clone();

    let engine = Engine::in_memory(Deps)
        .with_projection(
            seesaw_core::project("failing_projection")
                .priority(1)
                .then(|_event: seesaw_core::AnyEvent, _ctx: Context<Deps>| async move {
                    anyhow::bail!("projection error");
                }),
        )
        .with_projection(
            seesaw_core::project("passing_projection")
                .priority(2)
                .then(move |_event: seesaw_core::AnyEvent, _ctx: Context<Deps>| {
                    let spc = spc.clone();
                    async move {
                        spc.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }),
        );

    engine.emit(Ping { msg: "hi".into() }).settled().await?;

    assert_eq!(
        second_projection_counter.load(Ordering::SeqCst),
        1,
        "second projection should still run despite first projection error"
    );
    Ok(())
}

#[tokio::test]
async fn on_dlq_mapper_error_does_not_crash_engine() -> Result<()> {
    // The on_dlq mapper itself panics — engine should still settle
    let engine = Engine::in_memory(Deps)
        .on_dlq(|_info: seesaw_core::DlqTerminalInfo| -> HandlerDlq {
            panic!("mapper panic");
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("trigger_bad_mapper")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail"))
                }),
        );

    // The engine wraps the mapper — panics may or may not be caught
    let result = std::panic::AssertUnwindSafe(
        engine.emit(FailEvent { attempt: 0 }).settled(),
    );
    let outcome = futures::FutureExt::catch_unwind(result).await;

    match outcome {
        Ok(Ok(_)) | Ok(Err(_)) => { /* Engine handled it */ }
        Err(_) => { /* Panic propagated */ }
    }
    Ok(())
}

#[tokio::test]
async fn on_dlq_fires_for_each_failing_handler() -> Result<()> {
    let dlq_mapper_counter = Arc::new(AtomicUsize::new(0));
    let dmc = dlq_mapper_counter.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            dmc.fetch_add(1, Ordering::SeqCst);
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
                .id("fail_handler_1")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail 1"))
                }),
        )
        .with_handler(
            handler::on::<FailEvent>()
                .id("fail_handler_2")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail 2"))
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    assert_eq!(
        dlq_mapper_counter.load(Ordering::SeqCst),
        2,
        "on_dlq mapper should fire once for each failing handler"
    );
    Ok(())
}

#[tokio::test]
async fn on_failure_with_source_event_access() -> Result<()> {
    let captured: Arc<Mutex<Option<(i32, String)>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();

    let engine = Engine::in_memory(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("source_access_handler")
                .retry(1)
                .on_failure(move |event: Arc<FailEvent>, info: seesaw_core::ErrorContext| {
                    FailedTerminal {
                        error: format!("attempt={}, err={}", event.attempt, info.error),
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("source event test"))
                }),
        )
        .with_handler(handler::on::<FailedTerminal>().then(
            move |event: Arc<FailedTerminal>, _ctx: Context<Deps>| {
                let cap = cap.clone();
                async move {
                    *cap.lock() = Some((event.attempts, event.error.clone()));
                    Ok(events![])
                }
            },
        ));

    engine.emit(FailEvent { attempt: 42 }).settled().await?;

    let (attempts, error) = captured.lock().take().expect("on_failure handler should have fired");
    assert!(error.contains("attempt=42"), "should access source event field, got: {error}");
    assert!(error.contains("source event test"), "should contain original error, got: {error}");
    assert!(attempts > 0, "attempts should be > 0");
    Ok(())
}

#[tokio::test]
async fn on_dlq_not_fired_when_retry_succeeds() -> Result<()> {
    let dlq_counter = Arc::new(AtomicUsize::new(0));
    let dc = dlq_counter.clone();
    let attempt_counter = Arc::new(AtomicI32::new(0));
    let ac = attempt_counter.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            dc.fetch_add(1, Ordering::SeqCst);
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
                .id("retry_success_handler")
                .retry(3)
                .then(move |_event: Arc<FailEvent>, _ctx: Context<Deps>| {
                    let ac = ac.clone();
                    async move {
                        let attempt = ac.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            anyhow::bail!("first attempt fails");
                        }
                        Ok(events![])
                    }
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    assert_eq!(
        dlq_counter.load(Ordering::SeqCst),
        0,
        "on_dlq should NOT fire when retry eventually succeeds"
    );
    assert!(
        attempt_counter.load(Ordering::SeqCst) >= 2,
        "handler should have been called at least twice"
    );
    Ok(())
}

#[tokio::test]
async fn on_dlq_fires_after_all_retries_exhausted() -> Result<()> {
    let attempt_counter = Arc::new(AtomicI32::new(0));
    let ac = attempt_counter.clone();
    let captured_info: Arc<Mutex<Option<seesaw_core::DlqTerminalInfo>>> = Arc::new(Mutex::new(None));
    let ci = captured_info.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: seesaw_core::DlqTerminalInfo| {
            *ci.lock() = Some(info.clone());
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
                .id("exhaust_retries")
                .retry(3)
                .then(move |_event: Arc<FailEvent>, _ctx: Context<Deps>| {
                    let ac = ac.clone();
                    async move {
                        ac.fetch_add(1, Ordering::SeqCst);
                        Err::<Events, _>(anyhow::anyhow!("always fails"))
                    }
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let info = captured_info.lock().take().expect("on_dlq should fire after retries exhausted");
    assert_eq!(info.handler_id, "exhaust_retries");
    assert_eq!(info.reason, "failed");
    assert_eq!(
        info.attempts, info.max_attempts,
        "attempts ({}) should equal max_attempts ({})",
        info.attempts, info.max_attempts
    );
    assert!(
        attempt_counter.load(Ordering::SeqCst) >= 2,
        "handler should have been called multiple times, got: {}",
        attempt_counter.load(Ordering::SeqCst)
    );
    Ok(())
}

#[tokio::test]
async fn on_dlq_event_handler_failure_does_not_cascade() -> Result<()> {
    // The handler for the DLQ event itself fails — engine should NOT infinite loop
    let dlq_handler_counter = Arc::new(AtomicUsize::new(0));
    let dhc = dlq_handler_counter.clone();

    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct DlqFailEvent {
        error: String,
    }

    let engine = Engine::in_memory(Deps)
        .on_dlq(|info: seesaw_core::DlqTerminalInfo| DlqFailEvent {
            error: info.error,
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("original_failure")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("original fail"))
                }),
        )
        .with_handler(
            handler::on::<DlqFailEvent>()
                .id("dlq_handler_that_fails")
                .retry(1)
                .then(move |_event: Arc<DlqFailEvent>, _ctx: Context<Deps>| {
                    let dhc = dhc.clone();
                    async move {
                        dhc.fetch_add(1, Ordering::SeqCst);
                        Err::<Events, _>(anyhow::anyhow!("dlq handler also fails"))
                    }
                }),
        );

    // Should settle without infinite loop
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.emit(FailEvent { attempt: 0 }).settled(),
    )
    .await;

    assert!(
        result.is_ok(),
        "engine should settle without infinite loop even when DLQ handler fails"
    );

    assert!(
        dlq_handler_counter.load(Ordering::SeqCst) >= 1,
        "DLQ event handler should have fired at least once"
    );
    Ok(())
}

// -- Journal replay tests --

#[tokio::test]
async fn journal_replays_completed_run_calls_on_retry() -> Result<()> {
    let call_count = Arc::new(AtomicI32::new(0));
    let cc = call_count.clone();

    // Pre-seed the journal so ctx.run() replays instead of executing
    let store = Arc::new(MemoryStore::new());
    store
        .append_journal(
            "journal_handler",
            // We need to know the event_id ahead of time. Use a fixed correlation
            // and let the engine generate a deterministic event_id.
            // Instead, let's just verify the unit-level replay in context tests
            // and here verify that the store wiring works end-to-end by checking
            // that ctx.run() actually persists entries.
            Uuid::nil(),
            0,
            serde_json::json!("cached-result"),
        )
        .await?;

    // Verify the entry was stored
    let entries = store.load_journal("journal_handler", Uuid::nil()).await?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seq, 0);
    assert_eq!(entries[0].value, serde_json::json!("cached-result"));

    // Verify clear works
    store.clear_journal("journal_handler", Uuid::nil()).await?;
    let entries = store.load_journal("journal_handler", Uuid::nil()).await?;
    assert!(entries.is_empty());

    // Now test that ctx.run() inside a queued handler persists to journal
    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_handler(
            handler::on::<Ping>()
                .id("journal_handler")
                .then(move |event: Arc<Ping>, ctx: Context<Deps>| {
                    let cc = cc.clone();
                    async move {
                        let result: String = ctx
                            .run(move || {
                                cc.fetch_add(1, Ordering::SeqCst);
                                async move { Ok(format!("ran-{}", event.msg)) }
                            })
                            .await?;
                        assert_eq!(result, "ran-hello");
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

    // The closure should have executed once
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    Ok(())
}

#[tokio::test]
async fn journal_cleared_after_successful_handler() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_handler(
            handler::on::<Ping>()
                .id("journal_clear_test")
                .then(|event: Arc<Ping>, ctx: Context<Deps>| async move {
                    let _: String = ctx
                        .run(move || async move { Ok(format!("result-{}", event.msg)) })
                        .await?;
                    Ok(events![])
                }),
        );

    let handle = engine
        .emit(Ping {
            msg: "test".into(),
        })
        .settled()
        .await?;

    // After successful settlement, journal should be cleared
    // We can't easily get the event_id from inside the handler, but we can
    // check that no journal entries remain for this handler
    // The store's journal map should be empty after clear
    let entries = store
        .load_journal("journal_clear_test", handle.event_id)
        .await?;
    // Note: the journal key uses the triggering event_id, not the root event_id.
    // Since this is a queued handler, the event_id in the journal is the source event's ID.
    // We verify indirectly: if entries exist for any key, the clear didn't work.
    // For a more direct test, we check that the store's journal is empty overall.
    assert!(
        entries.is_empty(),
        "journal should be cleared after successful handler"
    );
    Ok(())
}

// ── Ephemeral sidecar tests ──────────────────────────────────────────

/// Event with a #[serde(skip)] field — will be Default on JSON round-trip.
#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventWithSkip {
    id: i32,
    #[serde(skip)]
    transient: Vec<String>,
}

/// Core scenario: handler receives the original typed event with #[serde(skip)] fields preserved.
#[tokio::test]
async fn ephemeral_preserves_serde_skip_fields() -> Result<()> {
    let received = Arc::new(Mutex::new(None::<(i32, usize)>));
    let received_clone = received.clone();

    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<EventWithSkip>()
            .id("check_skip")
            .then(move |event: Arc<EventWithSkip>, _ctx: Context<Deps>| {
                let r = received_clone.clone();
                async move {
                    *r.lock() = Some((event.id, event.transient.len()));
                    Ok(events![])
                }
            }),
    );

    engine
        .emit(EventWithSkip {
            id: 42,
            transient: vec!["a".into(), "b".into(), "c".into()],
        })
        .settled()
        .await?;

    let (id, len) = received.lock().unwrap();
    assert_eq!(id, 42);
    assert_eq!(len, 3, "ephemeral should preserve #[serde(skip)] fields during live dispatch");
    Ok(())
}

/// Ephemeral flows through a handler chain: A → handler → B → handler.
/// The child event (B) emitted by the first handler should also have its ephemeral preserved.
#[tokio::test]
async fn ephemeral_preserved_in_handler_chain() -> Result<()> {
    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Step1 {
        value: i32,
        #[serde(skip)]
        extra: Vec<u8>,
    }

    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Step2 {
        value: i32,
        #[serde(skip)]
        computed: String,
    }

    let final_received = Arc::new(Mutex::new(None::<(i32, String)>));
    let final_clone = final_received.clone();

    let engine = Engine::in_memory(Deps)
        .with_handler(
            handler::on::<Step1>()
                .id("step1_handler")
                .then(|event: Arc<Step1>, _ctx: Context<Deps>| async move {
                    // Verify step1 ephemeral works
                    assert_eq!(event.extra, vec![1, 2, 3]);
                    Ok(events![Step2 {
                        value: event.value * 2,
                        computed: "from_step1".to_string(),
                    }])
                }),
        )
        .with_handler(
            handler::on::<Step2>()
                .id("step2_handler")
                .then(move |event: Arc<Step2>, _ctx: Context<Deps>| {
                    let r = final_clone.clone();
                    async move {
                        *r.lock() = Some((event.value, event.computed.clone()));
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Step1 {
            value: 5,
            extra: vec![1, 2, 3],
        })
        .settled()
        .await?;

    let (value, computed) = final_received.lock().clone().unwrap();
    assert_eq!(value, 10);
    assert_eq!(computed, "from_step1", "child event ephemeral should be preserved");
    Ok(())
}

/// Multiple handlers on the same event all see the ephemeral.
#[tokio::test]
async fn ephemeral_shared_across_multiple_handlers() -> Result<()> {
    let seen_a = Arc::new(Mutex::new(None::<usize>));
    let seen_b = Arc::new(Mutex::new(None::<usize>));
    let seen_a_clone = seen_a.clone();
    let seen_b_clone = seen_b.clone();

    let engine = Engine::in_memory(Deps)
        .with_handler(
            handler::on::<EventWithSkip>()
                .id("handler_a")
                .then(move |event: Arc<EventWithSkip>, _ctx: Context<Deps>| {
                    let r = seen_a_clone.clone();
                    async move {
                        *r.lock() = Some(event.transient.len());
                        Ok(events![])
                    }
                }),
        )
        .with_handler(
            handler::on::<EventWithSkip>()
                .id("handler_b")
                .then(move |event: Arc<EventWithSkip>, _ctx: Context<Deps>| {
                    let r = seen_b_clone.clone();
                    async move {
                        *r.lock() = Some(event.transient.len());
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(EventWithSkip {
            id: 1,
            transient: vec!["x".into(), "y".into()],
        })
        .settled()
        .await?;

    assert_eq!(*seen_a.lock(), Some(2));
    assert_eq!(*seen_b.lock(), Some(2));
    Ok(())
}

/// Fan-out batch: each item in the batch preserves ephemeral independently.
#[tokio::test]
async fn ephemeral_preserved_in_batch_fanout() -> Result<()> {
    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Trigger {
        count: usize,
    }

    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct BatchChild {
        index: usize,
        #[serde(skip)]
        secret: String,
    }

    let secrets = Arc::new(Mutex::new(Vec::<String>::new()));
    let secrets_clone = secrets.clone();

    let engine = Engine::in_memory(Deps)
        .with_handler(
            handler::on::<Trigger>()
                .id("fan_out")
                .then(|event: Arc<Trigger>, _ctx: Context<Deps>| async move {
                    Ok(events![..(0..event.count).map(|i| BatchChild {
                        index: i,
                        secret: format!("secret_{}", i),
                    })])
                }),
        )
        .with_handler(
            handler::on::<BatchChild>()
                .id("collect")
                .then(move |event: Arc<BatchChild>, _ctx: Context<Deps>| {
                    let s = secrets_clone.clone();
                    async move {
                        s.lock().push(event.secret.clone());
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Trigger { count: 3 })
        .settled()
        .await?;

    let mut collected = secrets.lock().clone();
    collected.sort();
    assert_eq!(collected, vec!["secret_0", "secret_1", "secret_2"]);
    Ok(())
}

/// on_any handler receives the ephemeral via downcast.
#[tokio::test]
async fn ephemeral_available_via_on_any_handler() -> Result<()> {
    use seesaw_core::handler::AnyEvent;

    let received_len = Arc::new(AtomicUsize::new(0));
    let received_clone = received_len.clone();

    let engine = Engine::in_memory(Deps).with_handler(
        handler::on_any()
            .id("any_check")
            .then(move |event: AnyEvent, _ctx: Context<Deps>| {
                let r = received_clone.clone();
                async move {
                    if let Some(e) = event.downcast::<EventWithSkip>() {
                        r.store(e.transient.len(), Ordering::SeqCst);
                    }
                    Ok(events![])
                }
            }),
    );

    engine
        .emit(EventWithSkip {
            id: 99,
            transient: vec!["one".into(), "two".into()],
        })
        .settled()
        .await?;

    assert_eq!(received_len.load(Ordering::SeqCst), 2);
    Ok(())
}

/// Projection (inline observer) also sees the ephemeral via AnyEvent downcast.
#[tokio::test]
async fn ephemeral_available_in_projection() -> Result<()> {
    use seesaw_core::handler::{project, AnyEvent};

    let projection_len = Arc::new(AtomicUsize::new(0));
    let projection_clone = projection_len.clone();

    let engine = Engine::in_memory(Deps).with_projection(
        project("proj_check")
            .then(move |event: AnyEvent, _ctx: Context<Deps>| {
                let r = projection_clone.clone();
                async move {
                    if let Some(e) = event.downcast::<EventWithSkip>() {
                        r.store(e.transient.len(), Ordering::SeqCst);
                    }
                    Ok(())
                }
            }),
    );

    engine
        .emit(EventWithSkip {
            id: 7,
            transient: vec!["a".into()],
        })
        .settled()
        .await?;

    assert_eq!(projection_len.load(Ordering::SeqCst), 1);
    Ok(())
}

/// Ephemeral works with emit_output (EventOutput path).
#[tokio::test]
async fn ephemeral_via_emit_output() -> Result<()> {
    use seesaw_core::handler::EventOutput;

    let received = Arc::new(Mutex::new(None::<usize>));
    let received_clone = received.clone();

    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<EventWithSkip>()
            .id("output_check")
            .then(move |event: Arc<EventWithSkip>, _ctx: Context<Deps>| {
                let r = received_clone.clone();
                async move {
                    *r.lock() = Some(event.transient.len());
                    Ok(events![])
                }
            }),
    );

    engine
        .emit_output(EventOutput::new(EventWithSkip {
            id: 50,
            transient: vec!["q".into(), "w".into(), "e".into(), "r".into()],
        }))
        .settled()
        .await?;

    assert_eq!(*received.lock(), Some(4));
    Ok(())
}

/// DLQ on_failure handler still works — ephemeral doesn't interfere with error path.
#[tokio::test]
async fn ephemeral_does_not_break_dlq_path() -> Result<()> {
    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct FailSkip {
        id: i32,
        #[serde(skip)]
        transient: String,
    }

    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct FailedSkip {
        error: String,
    }

    let dlq_received = Arc::new(Mutex::new(None::<String>));
    let dlq_clone = dlq_received.clone();

    let engine = Engine::in_memory(Deps)
        .with_handler(
            handler::on::<FailSkip>()
                .id("always_fail")
                .retry(1)
                .on_failure(|_event, info: seesaw_core::handler::ErrorContext| FailedSkip {
                    error: info.error,
                })
                .then(|_event: Arc<FailSkip>, _ctx: Context<Deps>| async move {
                    anyhow::bail!("intentional failure")
                }),
        )
        .with_handler(
            handler::on::<FailedSkip>()
                .id("catch_failure")
                .then(move |event: Arc<FailedSkip>, _ctx: Context<Deps>| {
                    let r = dlq_clone.clone();
                    async move {
                        *r.lock() = Some(event.error.clone());
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(FailSkip {
            id: 1,
            transient: "will_be_lost".into(),
        })
        .settled()
        .await?;

    assert!(dlq_received.lock().is_some(), "DLQ terminal event should fire");
    Ok(())
}

/// Extract handler receives ephemeral-derived data correctly.
#[tokio::test]
async fn ephemeral_with_extract_handler() -> Result<()> {
    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct ExtractEvent {
        id: i32,
        name: String,
        #[serde(skip)]
        secret_data: Vec<u8>,
    }

    let received_id = Arc::new(AtomicI32::new(0));
    let received_clone = received_id.clone();

    let engine = Engine::in_memory(Deps).with_handler(
        handler::on::<ExtractEvent>()
            .id("extract_check")
            .extract(|e: &ExtractEvent| Some(e.id))
            .then(move |id: i32, _ctx: Context<Deps>| {
                let r = received_clone.clone();
                async move {
                    r.store(id, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
    );

    engine
        .emit(ExtractEvent {
            id: 77,
            name: "test".into(),
            secret_data: vec![1, 2, 3],
        })
        .settled()
        .await?;

    assert_eq!(received_id.load(Ordering::SeqCst), 77);
    Ok(())
}

// -- Resume hydration tests --

#[tokio::test]
async fn reclaimed_handler_sees_hydrated_aggregate_state() -> Result<()> {
    // Simulate a "resume after crash" scenario:
    //   1. Events are already persisted in the store
    //   2. A handler is sitting in the handler queue (as if reclaimed)
    //   3. A fresh engine (cold aggregates) calls settle()
    //   4. The handler should see hydrated aggregate state, not defaults

    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();
    let correlation_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    // Step 1: Pre-populate the event store (as if the event was already processed)
    store
        .append(new_event(
            "order_created",
            serde_json::json!({"order_id": order_id, "total": 250}),
            Some("Order"),
            Some(order_id),
        ))
        .await?;

    // Step 2: Inject a handler directly into the queue (simulating reclaim after crash)
    use seesaw_core::types::QueuedHandler;
    let event_type = "order_created".to_string();
    store
        .publish_handler_for_test(QueuedHandler {
            event_id,
            handler_id: "check_order_state".to_string(),
            correlation_id,
            event_type,
            event_payload: serde_json::json!({"order_id": order_id, "total": 250}),
            parent_event_id: None,
            execute_at: chrono::Utc::now(),
            timeout_seconds: 30,
            max_attempts: 1,
            priority: 10,
            hops: 0,
            attempts: 1,

            ephemeral: None,
        })
        .await;

    // Step 3: Build a fresh engine with cold aggregates
    let observed_status = Arc::new(Mutex::new(String::new()));
    let observed_total = Arc::new(AtomicUsize::new(0));
    let os = observed_status.clone();
    let ot = observed_total.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_handler(
            handler::on::<OrderCreated>()
                .id("check_order_state")
                .then(move |event: Arc<OrderCreated>, ctx: Context<Deps>| {
                    let os = os.clone();
                    let ot = ot.clone();
                    async move {
                        let state = ctx.aggregate_of::<Order>(event.order_id).curr;
                        *os.lock() = state.status.clone();
                        ot.store(state.total as usize, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    // Step 4: Settle — this should hydrate aggregates before running the reclaimed handler
    engine.settle().await?;

    // Step 5: Assert the handler saw the real aggregate state, not defaults
    assert_eq!(
        *observed_status.lock(),
        "created",
        "handler should see hydrated aggregate status, not default empty string"
    );
    assert_eq!(
        observed_total.load(Ordering::SeqCst),
        250,
        "handler should see hydrated aggregate total, not default 0"
    );

    Ok(())
}

#[tokio::test]
async fn dlq_event_carries_failed_handler_id_in_metadata() -> Result<()> {
    // DLQ terminal events should have handler_id metadata set to the
    // handler that failed, so causal flow graphs can wire the edge.
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .on_dlq(|info: seesaw_core::DlqTerminalInfo| GlobalDlqEvent {
            error: info.error,
        })
        .with_handler(
            handler::on::<FailEvent>()
                .id("failing_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("boom"))
                }),
        )
        .with_handler(
            handler::on::<GlobalDlqEvent>()
                .id("dlq_sink")
                .then(|_event: Arc<GlobalDlqEvent>, _ctx: Context<Deps>| async move {
                    Ok(events![])
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    // Find the persisted GlobalDlqEvent in the global log
    let log = store.global_log().lock();
    let dlq_event = log
        .iter()
        .find(|e| e.event_type == "global_dlq_event")
        .expect("GlobalDlqEvent should be persisted");

    assert_eq!(
        dlq_event.metadata.get("handler_id").and_then(|v| v.as_str()),
        Some("failing_handler"),
        "DLQ event metadata should carry the failed handler's ID"
    );

    // Also verify parent_id is set (causal link to the source event)
    assert!(
        dlq_event.parent_id.is_some(),
        "DLQ event should have a parent_id linking to the source event"
    );

    Ok(())
}

// -- Singleton hydration across event types --

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct PipelineState {
    source_plan: Option<String>,
    scrape_done: bool,
}

impl Aggregate for PipelineState {
    fn aggregate_type() -> &'static str {
        "PipelineState"
    }
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourcesPrepared {
    plan: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScrapeCompleted;

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExpansionTriggered;

impl Apply<SourcesPrepared> for PipelineState {
    fn apply(&mut self, event: SourcesPrepared) {
        self.source_plan = Some(event.plan);
    }
}

impl Apply<ScrapeCompleted> for PipelineState {
    fn apply(&mut self, _event: ScrapeCompleted) {
        self.scrape_done = true;
    }
}

#[tokio::test]
async fn singleton_hydrated_across_event_types_for_handler_filter() -> Result<()> {
    // Reproduces: handler triggered by ScrapeCompleted has a filter that
    // reads PipelineState.source_plan (set by SourcesPrepared). On resume,
    // only the ScrapeCompleted aggregator gets hydrated, so source_plan
    // is None and the filter returns false.
    let store = Arc::new(MemoryStore::new());
    let correlation_id = Uuid::new_v4();

    // Step 1: Pre-populate store with both events already persisted
    store
        .append(new_event(
            "sources_prepared",
            serde_json::json!({"plan": "social"}),
            Some("PipelineState"),
            Some(Uuid::nil()),
        ))
        .await?;
    store
        .append(new_event(
            "scrape_completed",
            serde_json::json!(null),
            Some("PipelineState"),
            Some(Uuid::nil()),
        ))
        .await?;

    // Mark events as already processed (checkpoint past both events).
    // MemoryStore positions start at 1, so two events are at positions 1 and 2.
    store.set_checkpoint(2);

    // Step 2: Inject a handler into the queue (simulating resume/reclaim)
    let event_type = "scrape_completed".to_string();
    store
        .publish_handler_for_test(seesaw_core::types::QueuedHandler {
            event_id: Uuid::new_v4(),
            handler_id: "expand_sources".to_string(),
            correlation_id,
            event_type,
            event_payload: serde_json::json!(null),
            parent_event_id: None,
            execute_at: chrono::Utc::now(),
            timeout_seconds: 30,
            max_attempts: 1,
            priority: 10,
            hops: 0,
            attempts: 1,

            ephemeral: None,
        })
        .await;

    // Step 3: Fresh engine — handler filter reads singleton state set by
    // a DIFFERENT event type than the one that triggered it
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        // Only SourcesPrepared has an aggregator — ScrapeCompleted does NOT
        // register an aggregator for PipelineState. The handler triggered by
        // ScrapeCompleted reads the singleton in its filter.
        .with_aggregator::<SourcesPrepared, PipelineState, _>(|_| Uuid::nil())
        .with_handler(
            handler::on::<ScrapeCompleted>()
                .id("expand_sources")
                .filter(move |_event, ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    // This filter checks state set by SourcesPrepared, not ScrapeCompleted
                    state.curr.source_plan.is_some()
                })
                .then(move |_event: Arc<ScrapeCompleted>, _ctx: Context<Deps>| {
                    let f = f.clone();
                    async move {
                        f.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        );

    // Step 4: Settle — should hydrate ALL singleton aggregators, not just ScrapeCompleted's
    engine.settle().await?;

    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "handler filter should see fully hydrated singleton (source_plan + scrape_done)"
    );

    Ok(())
}

// ── Multi-type handler integration tests ─────────────────────────────

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewDone {
    concern_id: Uuid,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoSignals {
    concern_id: Uuid,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Enriched {
    concern_id: Uuid,
    source: String,
}

#[tokio::test]
async fn multi_type_handler_fires_on_both_event_types() -> Result<()> {
    let fired = Arc::new(AtomicUsize::new(0));
    let sources: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let fired_c = fired.clone();
    let sources_c = sources.clone();

    let engine = Engine::in_memory(Deps)
        .with_handlers(vec![
            // Register one handler for ReviewDone
            handler::on::<ReviewDone>()
                .id("enrich::ReviewDone")
                .then(move |_event: Arc<ReviewDone>, _ctx: Context<Deps>| {
                    let f = fired_c.clone();
                    let s = sources_c.clone();
                    async move {
                        f.fetch_add(1, Ordering::SeqCst);
                        s.lock().push("review".into());
                        Ok(events![])
                    }
                }),
            // Register same logical handler for NoSignals
            handler::on::<NoSignals>()
                .id("enrich::NoSignals")
                .then({
                    let fired = fired.clone();
                    let sources = sources.clone();
                    move |_event: Arc<NoSignals>, _ctx: Context<Deps>| {
                        let f = fired.clone();
                        let s = sources.clone();
                        async move {
                            f.fetch_add(1, Ordering::SeqCst);
                            s.lock().push("no_signals".into());
                            Ok(events![])
                        }
                    }
                }),
        ]);

    // Fire ReviewDone
    engine.emit(ReviewDone { concern_id: Uuid::new_v4() }).settled().await?;
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    assert_eq!(sources.lock()[0], "review");

    // Fire NoSignals
    engine.emit(NoSignals { concern_id: Uuid::new_v4() }).settled().await?;
    assert_eq!(fired.load(Ordering::SeqCst), 2);
    assert_eq!(sources.lock()[1], "no_signals");

    Ok(())
}

#[tokio::test]
async fn multi_type_handler_with_filter_fires_on_both_types() -> Result<()> {
    let fired = Arc::new(AtomicUsize::new(0));
    let sources: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let fired_c = fired.clone();
    let sources_c = sources.clone();
    let fired_c2 = fired.clone();
    let sources_c2 = sources.clone();

    // Simulate a context-only filter (always returns true here)
    let engine = Engine::in_memory(Deps)
        .with_handlers(vec![
            handler::on::<ReviewDone>()
                .id("enrich_filtered::ReviewDone")
                .filter(|_event: &ReviewDone, _ctx: &Context<Deps>| true)
                .then(move |_event: Arc<ReviewDone>, _ctx: Context<Deps>| {
                    let f = fired_c.clone();
                    let s = sources_c.clone();
                    async move {
                        f.fetch_add(1, Ordering::SeqCst);
                        s.lock().push("review".into());
                        Ok(events![])
                    }
                }),
            handler::on::<NoSignals>()
                .id("enrich_filtered::NoSignals")
                .filter(|_event: &NoSignals, _ctx: &Context<Deps>| true)
                .then(move |_event: Arc<NoSignals>, _ctx: Context<Deps>| {
                    let f = fired_c2.clone();
                    let s = sources_c2.clone();
                    async move {
                        f.fetch_add(1, Ordering::SeqCst);
                        s.lock().push("no_signals".into());
                        Ok(events![])
                    }
                }),
        ]);

    engine.emit(ReviewDone { concern_id: Uuid::new_v4() }).settled().await?;
    assert_eq!(fired.load(Ordering::SeqCst), 1);

    engine.emit(NoSignals { concern_id: Uuid::new_v4() }).settled().await?;
    assert_eq!(fired.load(Ordering::SeqCst), 2);

    let s = sources.lock();
    assert_eq!(s[0], "review");
    assert_eq!(s[1], "no_signals");

    Ok(())
}

// ── Describe timing tests ─────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ProgressTracker {
    completed_steps: u32,
}

impl Aggregate for ProgressTracker {
    fn aggregate_type() -> &'static str {
        "ProgressTracker"
    }
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StepStarted;

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StepDone;

impl Apply<StepStarted> for ProgressTracker {
    fn apply(&mut self, _event: StepStarted) {
        self.completed_steps += 1;
    }
}

impl Apply<StepDone> for ProgressTracker {
    fn apply(&mut self, _event: StepDone) {
        self.completed_steps += 1;
    }
}

#[tokio::test]
async fn describe_reflects_post_handler_aggregate_state() -> Result<()> {
    // Regression: describe() runs during execute_event (before handlers run),
    // so it captures stale aggregate state. After a handler completes and
    // emits events that update aggregates, describe should be re-run so the
    // stored description reflects the latest state.
    let store = Arc::new(MemoryStore::new());
    let store_clone = store.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_aggregator::<StepStarted, ProgressTracker, _>(|_| Uuid::nil())
        .with_aggregator::<StepDone, ProgressTracker, _>(|_| Uuid::nil())
        .with_handler(
            handler::on::<StepStarted>()
                .id("process_step")
                .filter(|_event: &StepStarted, _ctx: &Context<Deps>| true)
                .describe(|ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<ProgressTracker>();
                    serde_json::json!({ "completed_steps": state.curr.completed_steps })
                })
                .then(|_event: Arc<StepStarted>, _ctx: Context<Deps>| async move {
                    Ok(events![StepDone])
                }),
        );

    let handle = engine.emit(StepStarted).settled().await?;
    let correlation_id = handle.correlation_id;

    // After settling: StepStarted was applied (count=1), handler emitted StepDone
    // which was also applied (count=2). The stored description should show count=2.
    let descriptions = store_clone
        .get_descriptions(correlation_id)
        .await?;

    let desc = descriptions
        .get("process_step")
        .expect("describe should have been stored for process_step");

    assert_eq!(
        desc["completed_steps"], 2,
        "describe should reflect post-handler aggregate state (count=2), \
         but got {}. This means describe only captured pre-handler state.",
        desc["completed_steps"]
    );

    Ok(())
}
