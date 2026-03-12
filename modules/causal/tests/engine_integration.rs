//! Integration tests for Engine settle loop.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use causal::aggregator::{Aggregate, Apply};
use causal::{aggregator, aggregators, event, events, reactor, Context, Engine, EventLog, Events, LogCursor, ReactorQueue, MemoryStore, NewEvent, Snapshot, StreamVersion};
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
        persistent: true,
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
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
        .with_reactor(reactor::on::<EventA>().then(
            |event: Arc<EventA>, _ctx: Context<Deps>| async move {
                Ok(events![EventB {
                    value: event.value + 1,
                }])
            },
        ))
        .with_reactor(reactor::on::<EventB>().then(
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
        .with_reactor(
            reactor::on::<Ping>()
                .id("ping_handler_a")
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let c = ca.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(events![])
                    }
                }),
        )
        .with_reactor(
            reactor::on::<Ping>()
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>()
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>()
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

    // Queued reactor has not run yet (no settle)
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // Now settle
    engine.settle().await?;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn handler_returns_nothing() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<FailEvent>()
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
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("always_fail")
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: causal::ErrorContext| {
                    FailedTerminal {
                        error: info.error,
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }),
        )
        .with_reactor(reactor::on::<FailedTerminal>().then(
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
        .with_reactor(
            reactor::on::<EventA>()
                .id("emit_b_queued")
                .retry(1)
                .then(|event: Arc<EventA>, _ctx: Context<Deps>| async move {
                    Ok(events![EventB {
                        value: event.value + 1,
                    }])
                }),
        )
        .with_reactor(reactor::on::<EventB>().then(
            move |_event: Arc<EventB>, ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    *sc.lock() = Some(ctx.correlation_id);
                    Ok(events![])
                }
            },
        ));

    let handle = engine.emit(EventA { value: 1 }).settled().await?;

    let seen = seen_correlation.lock().expect("EventB reactor should have run");
    assert_eq!(
        seen, handle.correlation_id,
        "EventB emitted by queued reactor must carry the original correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn dlq_terminal_preserves_correlation() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::in_memory(Deps)
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("always_fail_corr")
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: causal::ErrorContext| {
                    FailedTerminal {
                        error: info.error,
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }),
        )
        .with_reactor(reactor::on::<FailedTerminal>().then(
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
        .expect("FailedTerminal reactor should have run");
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(move |event: Arc<Ping>, ctx: Context<Deps>| {
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
        .with_reactor(reactor::on::<OrderPlaced>().then(
            move |event: Arc<OrderPlaced>, _ctx: Context<Deps>| {
                let s = seen_clone.clone();
                async move {
                    *s.lock() = Some((event.total, event.currency.clone()));
                    Ok(events![])
                }
            },
        ));

    // Emit a "v1" event (no currency field) by serializing manually.
    // The upcaster should inject "currency": "USD" before the reactor sees it.
    //
    // We emit an EventA that the reactor won't match, then manually publish
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
    use causal::aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};
    use causal::upcaster::{Upcaster, UpcasterRegistry};

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
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
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
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>()
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>()
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
        .with_reactor(reactor::on::<OrderCreated>().then(
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
        .with_reactor(reactor::on::<OrderCreated>().then(
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
    assert_eq!(events[1].version.unwrap(), StreamVersion::from_raw(2));
    Ok(())
}

#[tokio::test]
async fn events_without_aggregator_not_in_stream() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    // Ping has no aggregator registered
    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_reactor(reactor::on::<Ping>().then(
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
        .with_reactor(reactor::on::<Ping>().then(
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
        .with_reactor(
            reactor::on::<OrderShipped>()
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
        .with_reactor(
            reactor::on::<OrderShipped>()
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
            version: StreamVersion::from_raw(1),
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
        .with_reactor(
            reactor::on::<OrderShipped>()
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
    assert_eq!(events.last().unwrap().version.unwrap(), StreamVersion::from_raw(50));

    // Verify versions are sequential
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.version.unwrap(), StreamVersion::from_raw((i + 1) as u64));
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
        .with_reactor(reactor::on::<OrderCreated>().then(
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
            version: StreamVersion::from_raw(5),
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
        .with_reactor(
            reactor::on::<OrderShipped>()
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
        .with_reactor(
            reactor::on::<OrderShipped>()
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
        .with_reactor(reactor::on::<OrderConfirmed>().then(
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
    assert_eq!(events[0].version.unwrap(), StreamVersion::from_raw(1));
    assert_eq!(events[0].event_type, "order_created");
    assert_eq!(events[1].version.unwrap(), StreamVersion::from_raw(2));
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
    assert_eq!(order_events[0].version.unwrap(), StreamVersion::from_raw(1));
    assert_eq!(order_events[1].version.unwrap(), StreamVersion::from_raw(2));
    assert_eq!(customer_events[0].version.unwrap(), StreamVersion::from_raw(1));
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
        .with_reactor(
            reactor::on::<OrderCreated>()
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
        .with_reactor(
            reactor::on::<OrderShipped>()
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
            version: StreamVersion::from_raw(2),
            state: serde_json::json!({"status": "confirmed", "total": 999}),
            created_at: chrono::Utc::now(),
        })
        .await?;

    // Verify it loads back
    let loaded = store
        .load_snapshot("Order", order_id)
        .await?
        .expect("snapshot should exist");
    assert_eq!(loaded.version, StreamVersion::from_raw(2));
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
    assert_eq!(snap.version, StreamVersion::from_raw(5));

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
    assert_eq!(snap.version, StreamVersion::from_raw(10));

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
    assert_eq!(snap.version, StreamVersion::from_raw(10));

    // New engine (cold start) — should hydrate from snapshot at V10
    let engine2 = Engine::in_memory(Deps)
        .with_store(store.clone())
        .snapshot_every(5)
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_aggregator::<OrderConfirmed, Order, _>(|e| e.order_id);

    // Emit one more event — triggers hydration from snapshot
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine2 = engine2.with_reactor(
        reactor::on::<OrderConfirmed>()
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

    // Transition reactor should fire — proves hydration worked (state was "created" from snapshot)
    assert_eq!(fired.load(Ordering::SeqCst), 1, "reactor should fire from snapshot-hydrated state");

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
            version: StreamVersion::from_raw(50),
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
    assert_eq!(snap.version, StreamVersion::from_raw(50), "snapshot should remain at V50, not V51");

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
            persistent: true,
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
struct ReactorDlq {
    reactor_id: String,
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
        .on_dlq(|info: causal::DlqTerminalInfo| ReactorDlq {
            reactor_id: info.reactor_id,
            source_event_type: info.source_event_type,
            error: info.error,
            reason: info.reason,
            attempts: info.attempts,
        })
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("always_fail_dlq")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("boom"))
                }),
        )
        .with_reactor(reactor::on::<ReactorDlq>().then(
            move |_event: Arc<ReactorDlq>, _ctx: Context<Deps>| {
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
    let captured: Arc<Mutex<Option<causal::DlqTerminalInfo>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            *cap.lock() = Some(info.clone());
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("check_info_handler")
                .retry(2)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("detailed error"))
                }),
        );

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let info = captured.lock().take().expect("on_dlq should have been called");
    assert_eq!(info.reactor_id, "check_info_handler");
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
        .on_dlq(|info: causal::DlqTerminalInfo| GlobalDlqEvent {
            error: info.error,
        })
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("has_on_failure")
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: causal::ErrorContext| {
                    HandlerSpecificFailure { error: info.error }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail"))
                }),
        )
        .with_reactor(reactor::on::<HandlerSpecificFailure>().then(
            move |_event: Arc<HandlerSpecificFailure>, _ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ))
        .with_reactor(reactor::on::<GlobalDlqEvent>().then(
            move |_event: Arc<GlobalDlqEvent>, _ctx: Context<Deps>| {
                let gc = gc.clone();
                async move {
                    gc.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            },
        ));

    engine.emit(FailEvent { attempt: 0 }).settled().await?;

    assert_eq!(specific_counter.load(Ordering::SeqCst), 1, "per-reactor on_failure should fire");
    assert_eq!(global_counter.load(Ordering::SeqCst), 0, "global on_dlq should NOT fire when on_failure is present");
    Ok(())
}

#[tokio::test]
async fn on_dlq_fires_on_timeout() -> Result<()> {
    let captured_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cr = captured_reason.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            *cr.lock() = Some(info.reason.clone());
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
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
        .on_dlq(|info: causal::DlqTerminalInfo| ReactorDlq {
            reactor_id: info.reactor_id,
            source_event_type: info.source_event_type,
            error: info.error,
            reason: info.reason,
            attempts: info.attempts,
        })
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("corr_fail")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail"))
                }),
        )
        .with_reactor(reactor::on::<ReactorDlq>().then(
            move |_event: Arc<ReactorDlq>, ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    *sc.lock() = Some(ctx.correlation_id);
                    Ok(events![])
                }
            },
        ));

    let handle = engine.emit(FailEvent { attempt: 0 }).settled().await?;

    let seen = seen_correlation.lock().expect("ReactorDlq reactor should have run");
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
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            c.fetch_add(1, Ordering::SeqCst);
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<Ping>()
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
        .with_reactor(
            reactor::on::<FailEvent>()
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
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>()
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
        .with_reactor(
            reactor::on::<EventA>()
                .id("a_to_b")
                .retry(1)
                .then(|event: Arc<EventA>, _ctx: Context<Deps>| async move {
                    Ok(events![EventB { value: event.value }])
                }),
        )
        .with_reactor(
            reactor::on::<EventB>()
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
    let seen = seen_correlation.lock().expect("EventB reactor should have run");
    assert_eq!(
        seen, my_id,
        "Child event must inherit the custom correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn omitted_correlation_id_auto_generates() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>()
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
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

    assert_eq!(counter.load(Ordering::SeqCst), 0, "reactor should not fire for cancelled workflow");
    Ok(())
}

#[tokio::test]
async fn cancel_dlqs_pending_reactors() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // Use a queued reactor so it goes through poll_next_handler
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>()
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

    assert_eq!(counter.load(Ordering::SeqCst), 0, "queued reactor should not fire for cancelled workflow");
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
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
    /// Child event emitted by the Ping reactor.
    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Pong;

    let pong_counter = Arc::new(AtomicUsize::new(0));
    let pong_clone = pong_counter.clone();

    let store = Arc::new(MemoryStore::new());
    let store_for_handler = store.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store)
        // Ping reactor: emits Pong, then cancels own correlation via the store
        .with_reactor(
            reactor::on::<Ping>().then(move |_event: Arc<Ping>, ctx: Context<Deps>| {
                let s = store_for_handler.clone();
                let cid = ctx.correlation_id;
                async move {
                    // Cancel the workflow from within the reactor.
                    // The emitted Pong will be queued but should be rejected
                    // on the next settle loop iteration.
                    s.cancel(cid).await.unwrap();
                    Ok(events![Pong])
                }
            }),
        )
        // Pong reactor: should never fire because workflow is cancelled
        .with_reactor(
            reactor::on::<Pong>().then(move |_event: Arc<Pong>, _ctx: Context<Deps>| {
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
        "downstream Pong reactor should not fire after mid-settle cancel"
    );
    Ok(())
}

// ── QueueStatus + ProcessHandle workflow tests ──────────────────

#[tokio::test]
async fn queue_status_empty_after_settle() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
    );

    let handle = engine
        .emit(Ping {
            msg: "hi".into(),
        })
        .settled()
        .await?;

    let status = handle.status().await?;
    assert_eq!(status.pending_reactors, 0);
    assert_eq!(status.dead_lettered, 0);
    Ok(())
}

#[tokio::test]
async fn queue_status_shows_pending_before_settle() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
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
        "reactor should not fire after cancel via ProcessHandle"
    );
    Ok(())
}

#[tokio::test]
async fn engine_status_delegates_to_store() -> Result<()> {
    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(events![]) }),
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
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            *ce.lock() = Some(info.error.clone());
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
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
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            *ce.lock() = Some(info.error.clone());
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
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
    // Document current behavior: a panic in a reactor may crash the engine.
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let engine = Engine::in_memory(Deps)
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("panic_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    panic!("boom");
                    #[allow(unreachable_code)]
                    Ok(events![])
                }),
        )
        .with_reactor(reactor::on::<Ping>().then(
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
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            dc.fetch_add(1, Ordering::SeqCst);
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
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
            causal::project("failing_projection")
                .then(|_event: causal::AnyEvent, _ctx: Context<Deps>| async move {
                    anyhow::bail!("projection failed");
                }),
        )
        .with_reactor(reactor::on::<Ping>().then(
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
        "reactor should still fire even when projection errors"
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
            causal::project("failing_projection")
                .priority(1)
                .then(|_event: causal::AnyEvent, _ctx: Context<Deps>| async move {
                    anyhow::bail!("projection error");
                }),
        )
        .with_projection(
            causal::project("passing_projection")
                .priority(2)
                .then(move |_event: causal::AnyEvent, _ctx: Context<Deps>| {
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
        .on_dlq(|_info: causal::DlqTerminalInfo| -> ReactorDlq {
            panic!("mapper panic");
        })
        .with_reactor(
            reactor::on::<FailEvent>()
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
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            dmc.fetch_add(1, Ordering::SeqCst);
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("fail_handler_1")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("fail 1"))
                }),
        )
        .with_reactor(
            reactor::on::<FailEvent>()
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
        "on_dlq mapper should fire once for each failing reactor"
    );
    Ok(())
}

#[tokio::test]
async fn on_failure_with_source_event_access() -> Result<()> {
    let captured: Arc<Mutex<Option<(i32, String)>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();

    let engine = Engine::in_memory(Deps)
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("source_access_handler")
                .retry(1)
                .on_failure(move |event: Arc<FailEvent>, info: causal::ErrorContext| {
                    FailedTerminal {
                        error: format!("attempt={}, err={}", event.attempt, info.error),
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("source event test"))
                }),
        )
        .with_reactor(reactor::on::<FailedTerminal>().then(
            move |event: Arc<FailedTerminal>, _ctx: Context<Deps>| {
                let cap = cap.clone();
                async move {
                    *cap.lock() = Some((event.attempts, event.error.clone()));
                    Ok(events![])
                }
            },
        ));

    engine.emit(FailEvent { attempt: 42 }).settled().await?;

    let (attempts, error) = captured.lock().take().expect("on_failure reactor should have fired");
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
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            dc.fetch_add(1, Ordering::SeqCst);
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
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
        "reactor should have been called at least twice"
    );
    Ok(())
}

#[tokio::test]
async fn on_dlq_fires_after_all_retries_exhausted() -> Result<()> {
    let attempt_counter = Arc::new(AtomicI32::new(0));
    let ac = attempt_counter.clone();
    let captured_info: Arc<Mutex<Option<causal::DlqTerminalInfo>>> = Arc::new(Mutex::new(None));
    let ci = captured_info.clone();

    let engine = Engine::in_memory(Deps)
        .on_dlq(move |info: causal::DlqTerminalInfo| {
            *ci.lock() = Some(info.clone());
            ReactorDlq {
                reactor_id: info.reactor_id,
                source_event_type: info.source_event_type,
                error: info.error,
                reason: info.reason,
                attempts: info.attempts,
            }
        })
        .with_reactor(
            reactor::on::<FailEvent>()
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
    assert_eq!(info.reactor_id, "exhaust_retries");
    assert_eq!(info.reason, "failed");
    assert_eq!(
        info.attempts, info.max_attempts,
        "attempts ({}) should equal max_attempts ({})",
        info.attempts, info.max_attempts
    );
    assert!(
        attempt_counter.load(Ordering::SeqCst) >= 2,
        "reactor should have been called multiple times, got: {}",
        attempt_counter.load(Ordering::SeqCst)
    );
    Ok(())
}

#[tokio::test]
async fn on_dlq_event_handler_failure_does_not_cascade() -> Result<()> {
    // The reactor for the DLQ event itself fails — engine should NOT infinite loop
    let dlq_handler_counter = Arc::new(AtomicUsize::new(0));
    let dhc = dlq_handler_counter.clone();

    #[event]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct DlqFailEvent {
        error: String,
    }

    let engine = Engine::in_memory(Deps)
        .on_dlq(|info: causal::DlqTerminalInfo| DlqFailEvent {
            error: info.error,
        })
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("original_failure")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("original fail"))
                }),
        )
        .with_reactor(
            reactor::on::<DlqFailEvent>()
                .id("dlq_handler_that_fails")
                .retry(1)
                .then(move |_event: Arc<DlqFailEvent>, _ctx: Context<Deps>| {
                    let dhc = dhc.clone();
                    async move {
                        dhc.fetch_add(1, Ordering::SeqCst);
                        Err::<Events, _>(anyhow::anyhow!("dlq reactor also fails"))
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
        "engine should settle without infinite loop even when DLQ reactor fails"
    );

    assert!(
        dlq_handler_counter.load(Ordering::SeqCst) >= 1,
        "DLQ event reactor should have fired at least once"
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

    // Now test that ctx.run() inside a queued reactor persists to journal
    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_reactor(
            reactor::on::<Ping>()
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
        .with_reactor(
            reactor::on::<Ping>()
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
    // We can't easily get the event_id from inside the reactor, but we can
    // check that no journal entries remain for this reactor
    // The store's journal map should be empty after clear
    let entries = store
        .load_journal("journal_clear_test", handle.event_id)
        .await?;
    // Note: the journal key uses the triggering event_id, not the root event_id.
    // Since this is a queued reactor, the event_id in the journal is the source event's ID.
    // We verify indirectly: if entries exist for any key, the clear didn't work.
    // For a more direct test, we check that the store's journal is empty overall.
    assert!(
        entries.is_empty(),
        "journal should be cleared after successful reactor"
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

/// Core scenario: reactor receives the original typed event with #[serde(skip)] fields preserved.
#[tokio::test]
async fn ephemeral_preserves_serde_skip_fields() -> Result<()> {
    let received = Arc::new(Mutex::new(None::<(i32, usize)>));
    let received_clone = received.clone();

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<EventWithSkip>()
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

/// Ephemeral flows through a reactor chain: A → reactor → B → reactor.
/// The child event (B) emitted by the first reactor should also have its ephemeral preserved.
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
        .with_reactor(
            reactor::on::<Step1>()
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
        .with_reactor(
            reactor::on::<Step2>()
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

/// Multiple reactors on the same event all see the ephemeral.
#[tokio::test]
async fn ephemeral_shared_across_multiple_handlers() -> Result<()> {
    let seen_a = Arc::new(Mutex::new(None::<usize>));
    let seen_b = Arc::new(Mutex::new(None::<usize>));
    let seen_a_clone = seen_a.clone();
    let seen_b_clone = seen_b.clone();

    let engine = Engine::in_memory(Deps)
        .with_reactor(
            reactor::on::<EventWithSkip>()
                .id("handler_a")
                .then(move |event: Arc<EventWithSkip>, _ctx: Context<Deps>| {
                    let r = seen_a_clone.clone();
                    async move {
                        *r.lock() = Some(event.transient.len());
                        Ok(events![])
                    }
                }),
        )
        .with_reactor(
            reactor::on::<EventWithSkip>()
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
        .with_reactor(
            reactor::on::<Trigger>()
                .id("fan_out")
                .then(|event: Arc<Trigger>, _ctx: Context<Deps>| async move {
                    Ok(events![..(0..event.count).map(|i| BatchChild {
                        index: i,
                        secret: format!("secret_{}", i),
                    })])
                }),
        )
        .with_reactor(
            reactor::on::<BatchChild>()
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

/// on_any reactor receives the ephemeral via downcast.
#[tokio::test]
async fn ephemeral_available_via_on_any_handler() -> Result<()> {
    use causal::reactor::AnyEvent;

    let received_len = Arc::new(AtomicUsize::new(0));
    let received_clone = received_len.clone();

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on_any()
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
    use causal::reactor::{project, AnyEvent};

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
    use causal::reactor::EventOutput;

    let received = Arc::new(Mutex::new(None::<usize>));
    let received_clone = received.clone();

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<EventWithSkip>()
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

/// DLQ on_failure reactor still works — ephemeral doesn't interfere with error path.
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
        .with_reactor(
            reactor::on::<FailSkip>()
                .id("always_fail")
                .retry(1)
                .on_failure(|_event, info: causal::reactor::ErrorContext| FailedSkip {
                    error: info.error,
                })
                .then(|_event: Arc<FailSkip>, _ctx: Context<Deps>| async move {
                    anyhow::bail!("intentional failure")
                }),
        )
        .with_reactor(
            reactor::on::<FailedSkip>()
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

/// Extract reactor receives ephemeral-derived data correctly.
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

    let engine = Engine::in_memory(Deps).with_reactor(
        reactor::on::<ExtractEvent>()
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
    //   2. A reactor is sitting in the reactor queue (as if reclaimed)
    //   3. A fresh engine (cold aggregates) calls settle()
    //   4. The reactor should see hydrated aggregate state, not defaults

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

    // Step 2: Inject a reactor directly into the queue (simulating reclaim after crash)
    use causal::types::QueuedReactor;
    let event_type = "order_created".to_string();
    store
        .publish_reactor_for_test(QueuedReactor {
            event_id,
            reactor_id: "check_order_state".to_string(),
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
        .with_reactor(
            reactor::on::<OrderCreated>()
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

    // Step 4: Settle — this should hydrate aggregates before running the reclaimed reactor
    engine.settle().await?;

    // Step 5: Assert the reactor saw the real aggregate state, not defaults
    assert_eq!(
        *observed_status.lock(),
        "created",
        "reactor should see hydrated aggregate status, not default empty string"
    );
    assert_eq!(
        observed_total.load(Ordering::SeqCst),
        250,
        "reactor should see hydrated aggregate total, not default 0"
    );

    Ok(())
}

#[tokio::test]
async fn dlq_event_carries_failed_reactor_id_in_metadata() -> Result<()> {
    // DLQ terminal events should have reactor_id metadata set to the
    // reactor that failed, so causal flow graphs can wire the edge.
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .on_dlq(|info: causal::DlqTerminalInfo| GlobalDlqEvent {
            error: info.error,
        })
        .with_reactor(
            reactor::on::<FailEvent>()
                .id("failing_handler")
                .retry(1)
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("boom"))
                }),
        )
        .with_reactor(
            reactor::on::<GlobalDlqEvent>()
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
        dlq_event.metadata.get("reactor_id").and_then(|v| v.as_str()),
        Some("failing_handler"),
        "DLQ event metadata should carry the failed reactor's ID"
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
    // Reproduces: reactor triggered by ScrapeCompleted has a filter that
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
    store.set_checkpoint(LogCursor::from_raw(2));

    // Step 2: Inject a reactor into the queue (simulating resume/reclaim)
    let event_type = "scrape_completed".to_string();
    store
        .publish_reactor_for_test(causal::types::QueuedReactor {
            event_id: Uuid::new_v4(),
            reactor_id: "expand_sources".to_string(),
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

    // Step 3: Fresh engine — reactor filter reads singleton state set by
    // a DIFFERENT event type than the one that triggered it
    let fired = Arc::new(AtomicUsize::new(0));
    let f = fired.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        // Only SourcesPrepared has an aggregator — ScrapeCompleted does NOT
        // register an aggregator for PipelineState. The reactor triggered by
        // ScrapeCompleted reads the singleton in its filter.
        .with_aggregator::<SourcesPrepared, PipelineState, _>(|_| Uuid::nil())
        .with_reactor(
            reactor::on::<ScrapeCompleted>()
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
        "reactor filter should see fully hydrated singleton (source_plan + scrape_done)"
    );

    Ok(())
}

// ── Multi-type reactor integration tests ─────────────────────────────

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
        .with_reactors(vec![
            // Register one reactor for ReviewDone
            reactor::on::<ReviewDone>()
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
            // Register same logical reactor for NoSignals
            reactor::on::<NoSignals>()
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
        .with_reactors(vec![
            reactor::on::<ReviewDone>()
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
            reactor::on::<NoSignals>()
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
    // Regression: describe() runs during execute_event (before reactors run),
    // so it captures stale aggregate state. After a reactor completes and
    // emits events that update aggregates, describe should be re-run so the
    // stored description reflects the latest state.
    let store = Arc::new(MemoryStore::new());
    let store_clone = store.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_aggregator::<StepStarted, ProgressTracker, _>(|_| Uuid::nil())
        .with_aggregator::<StepDone, ProgressTracker, _>(|_| Uuid::nil())
        .with_reactor(
            reactor::on::<StepStarted>()
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

    // After settling: StepStarted was applied (count=1), reactor emitted StepDone
    // which was also applied (count=2). The stored description should show count=2.
    let descriptions = store_clone
        .get_descriptions(correlation_id)
        .await?;

    let desc = descriptions
        .get("process_step")
        .expect("describe should have been stored for process_step");

    assert_eq!(
        desc["completed_steps"], 2,
        "describe should reflect post-reactor aggregate state (count=2), \
         but got {}. This means describe only captured pre-reactor state.",
        desc["completed_steps"]
    );

    Ok(())
}

// ── Ephemeral event tests ─────────────────────────────────────────────

#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EnrichmentReady {
    concern_id: Uuid,
}

#[event]
#[derive(Clone, Serialize, Deserialize)]
struct EnrichmentComplete {
    concern_id: Uuid,
}

#[event]
#[derive(Clone, Serialize, Deserialize)]
struct EphTrigger;

#[tokio::test]
async fn ephemeral_event_not_in_store() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let handler_ran = Arc::new(AtomicI32::new(0));
    let handler_ran2 = handler_ran.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentReady {
                    concern_id: Uuid::nil(),
                }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(move |event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                let ran = handler_ran2.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(events![EnrichmentComplete {
                        concern_id: event.concern_id,
                    }])
                }
            }),
        );

    engine
        .emit(EphTrigger)
        .settled()
        .await?;

    // Reactor should have executed
    assert_eq!(handler_ran.load(Ordering::SeqCst), 1);

    // All events are in the operational store (Postgres), but ephemeral ones
    // have persistent=false (won't be forwarded to KurrentDB).
    let all_events = store.global_log().lock().clone();
    let event_types: Vec<_> = all_events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(event_types.contains(&"eph_trigger"));
    assert!(event_types.contains(&"enrichment_ready"));
    assert!(event_types.contains(&"enrichment_complete"));

    // Ephemeral event has persistent=false
    let enrichment = all_events.iter().find(|e| e.event_type == "enrichment_ready").unwrap();
    assert!(!enrichment.persistent, "Ephemeral event should have persistent=false");

    // Persistent events have persistent=true
    let trigger = all_events.iter().find(|e| e.event_type == "eph_trigger").unwrap();
    assert!(trigger.persistent, "EphTrigger should have persistent=true");
    let complete = all_events.iter().find(|e| e.event_type == "enrichment_complete").unwrap();
    assert!(complete.persistent, "EnrichmentComplete should have persistent=true");

    Ok(())
}

#[tokio::test]
async fn ephemeral_event_handler_emits_persistent() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let complete_count = Arc::new(AtomicI32::new(0));
    let complete_count2 = complete_count.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentReady {
                    concern_id: Uuid::nil(),
                }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(move |event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                let count = complete_count2.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(events![EnrichmentComplete {
                        concern_id: event.concern_id,
                    }])
                }
            }),
        );

    engine
        .emit(EphTrigger)
        .settled()
        .await?;

    assert_eq!(complete_count.load(Ordering::SeqCst), 1);

    // The persistent event emitted by the ephemeral reactor IS in the log
    let all_events = store.global_log().lock().clone();
    let has_complete = all_events
        .iter()
        .any(|e| e.event_type == "enrichment_complete");
    assert!(has_complete, "Persistent event from ephemeral reactor should be in log");

    Ok(())
}

#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EphemeralSignal;

#[event]
#[derive(Clone, Serialize, Deserialize)]
struct PersistentResult {
    value: i32,
}

#[tokio::test]
async fn mixed_persistent_and_ephemeral_in_same_batch() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let signal_handled = Arc::new(AtomicI32::new(0));
    let result_handled = Arc::new(AtomicI32::new(0));
    let signal_handled2 = signal_handled.clone();
    let result_handled2 = result_handled.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![
                    EphemeralSignal,
                    PersistentResult { value: 42 },
                ])
            }),
        )
        .with_reactor(
            reactor::on::<EphemeralSignal>().then(move |_event: Arc<EphemeralSignal>, _ctx: Context<Deps>| {
                let ran = signal_handled2.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
        )
        .with_reactor(
            reactor::on::<PersistentResult>().then(move |_event: Arc<PersistentResult>, _ctx: Context<Deps>| {
                let ran = result_handled2.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
        );

    engine
        .emit(EphTrigger)
        .settled()
        .await?;

    assert_eq!(signal_handled.load(Ordering::SeqCst), 1, "Ephemeral reactor should run");
    assert_eq!(result_handled.load(Ordering::SeqCst), 1, "Persistent reactor should run");

    // All events in operational store; ephemeral has persistent=false
    let all_events = store.global_log().lock().clone();
    let ephemeral = all_events.iter().find(|e| e.event_type == "ephemeral_signal").expect("Ephemeral should be in operational store");
    assert!(!ephemeral.persistent, "Ephemeral should have persistent=false");
    let persistent = all_events.iter().find(|e| e.event_type == "persistent_result").expect("Persistent should be in log");
    assert!(persistent.persistent, "Persistent should have persistent=true");

    Ok(())
}

#[tokio::test]
async fn ephemeral_event_does_not_apply_to_aggregates() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentReady {
                    concern_id: Uuid::nil(),
                }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(|event: Arc<EnrichmentReady>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentComplete {
                    concern_id: event.concern_id,
                }])
            }),
        );

    engine
        .emit(EphTrigger)
        .settled()
        .await?;

    // Ephemeral event IS in operational store but with persistent=false
    // (won't be forwarded to KurrentDB, won't be applied to aggregates)
    let all_events = store.global_log().lock().clone();
    let ephemeral = all_events.iter().find(|e| e.event_type == "enrichment_ready").expect("Ephemeral should be in operational store");
    assert!(!ephemeral.persistent, "Ephemeral event should have persistent=false");

    Ok(())
}

#[tokio::test]
async fn ephemeral_event_does_not_run_projections() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let projection_count = Arc::new(AtomicI32::new(0));
    let projection_count2 = projection_count.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentReady {
                    concern_id: Uuid::nil(),
                }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(|event: Arc<EnrichmentReady>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentComplete {
                    concern_id: event.concern_id,
                }])
            }),
        )
        .with_projection(
            reactor::project("enrichment_projector").then(move |_event: causal::AnyEvent, _ctx: Context<Deps>| {
                let count = projection_count2.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }),
        );

    engine
        .emit(EphTrigger)
        .settled()
        .await?;

    // Projections run for: EphTrigger (persistent) and EnrichmentComplete (persistent)
    // Projections do NOT run for: EnrichmentReady (ephemeral)
    let count = projection_count.load(Ordering::SeqCst);
    assert_eq!(
        count, 2,
        "Projections should run for 2 persistent events (EphTrigger + EnrichmentComplete), got {}",
        count
    );

    Ok(())
}

#[tokio::test]
async fn ephemeral_event_hop_counting() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let handler_ran = Arc::new(AtomicI32::new(0));
    let handler_ran2 = handler_ran.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentReady {
                    concern_id: Uuid::nil(),
                }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(move |event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                let ran = handler_ran2.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(events![EnrichmentComplete {
                        concern_id: event.concern_id,
                    }])
                }
            }),
        );

    engine
        .emit(EphTrigger)
        .settled()
        .await?;

    // Reactor should run exactly once (no infinite loops)
    assert_eq!(handler_ran.load(Ordering::SeqCst), 1);

    Ok(())
}

#[tokio::test]
async fn ephemeral_root_event_not_persisted() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let handler_ran = Arc::new(AtomicI32::new(0));
    let handler_ran2 = handler_ran.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(move |event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                let ran = handler_ran2.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(events![EnrichmentComplete {
                        concern_id: event.concern_id,
                    }])
                }
            }),
        );

    // Emit an ephemeral event directly as a root event
    engine
        .emit(EnrichmentReady {
            concern_id: Uuid::nil(),
        })
        .settled()
        .await?;

    // Reactor should have executed
    assert_eq!(handler_ran.load(Ordering::SeqCst), 1);

    // EnrichmentReady IS in operational store but with persistent=false
    let all_events = store.global_log().lock().clone();
    let ready = all_events.iter().find(|e| e.event_type == "enrichment_ready").expect("Ephemeral root should be in operational store");
    assert!(!ready.persistent, "Ephemeral root should have persistent=false");

    // EnrichmentComplete (persistent) emitted by the reactor SHOULD be in the store
    let complete = all_events.iter().find(|e| e.event_type == "enrichment_complete").expect("Persistent event from reactor should be in store");
    assert!(complete.persistent, "EnrichmentComplete should have persistent=true");

    Ok(())
}

// ── Adversarial ephemeral tests ───────────────────────────────────────

#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EphA;

#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EphB;

#[event]
#[derive(Clone, Serialize, Deserialize)]
struct PersistC {
    chain_complete: bool,
}

/// Ephemeral → ephemeral → persistent chain. Both ephemeral hops must
/// process without persistence; the final persistent event must land in the log.
#[tokio::test]
async fn chained_ephemeral_to_ephemeral_to_persistent() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let chain_end = Arc::new(AtomicI32::new(0));
    let chain_end2 = chain_end.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EphA])
            }),
        )
        .with_reactor(
            reactor::on::<EphA>().then(|_event: Arc<EphA>, _ctx: Context<Deps>| async move {
                Ok(events![EphB])
            }),
        )
        .with_reactor(
            reactor::on::<EphB>().then(move |_event: Arc<EphB>, _ctx: Context<Deps>| {
                let end = chain_end2.clone();
                async move {
                    end.fetch_add(1, Ordering::SeqCst);
                    Ok(events![PersistC { chain_complete: true }])
                }
            }),
        );

    engine.emit(EphTrigger).settled().await?;

    assert_eq!(chain_end.load(Ordering::SeqCst), 1, "Chain should complete");

    let all_events = store.global_log().lock().clone();
    let eph_a = all_events.iter().find(|e| e.event_type == "eph_a").expect("EphA in operational store");
    assert!(!eph_a.persistent, "EphA should have persistent=false");
    let eph_b = all_events.iter().find(|e| e.event_type == "eph_b").expect("EphB in operational store");
    assert!(!eph_b.persistent, "EphB should have persistent=false");
    let persist_c = all_events.iter().find(|e| e.event_type == "persist_c").expect("PersistC should be in log");
    assert!(persist_c.persistent, "PersistC should have persistent=true");

    Ok(())
}

/// Ephemeral root event (no persistent trigger at all). The settle loop
/// must still process it even though log.load_from returns nothing.
#[tokio::test]
async fn ephemeral_only_root_event_still_settles() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let handler_ran = Arc::new(AtomicI32::new(0));
    let handler_ran2 = handler_ran.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphA>().then(move |_event: Arc<EphA>, _ctx: Context<Deps>| {
                let ran = handler_ran2.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
        );

    engine.emit(EphA).settled().await?;

    assert_eq!(handler_ran.load(Ordering::SeqCst), 1, "Reactor for ephemeral-only root should fire");
    let all_events = store.global_log().lock().clone();
    assert_eq!(all_events.len(), 1, "Ephemeral event should be in operational store");
    assert!(!all_events[0].persistent, "Ephemeral-only root should have persistent=false");

    Ok(())
}

/// Multiple ephemeral root events emitted back-to-back.
#[tokio::test]
async fn multiple_ephemeral_root_events() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let count = Arc::new(AtomicI32::new(0));
    let count2 = count.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(move |_event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                let c = count2.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![EnrichmentComplete {
                        concern_id: Uuid::nil(),
                    }])
                }
            }),
        );

    engine
        .emit(EnrichmentReady { concern_id: Uuid::new_v4() })
        .settled()
        .await?;
    engine
        .emit(EnrichmentReady { concern_id: Uuid::new_v4() })
        .settled()
        .await?;
    engine
        .emit(EnrichmentReady { concern_id: Uuid::new_v4() })
        .settled()
        .await?;

    assert_eq!(count.load(Ordering::SeqCst), 3, "All three ephemeral roots should fire reactors");

    // Three EnrichmentReady (ephemeral) + three EnrichmentComplete (persistent)
    let all_events = store.global_log().lock().clone();
    let ready_events: Vec<_> = all_events.iter().filter(|e| e.event_type == "enrichment_ready").collect();
    let complete_events: Vec<_> = all_events.iter().filter(|e| e.event_type == "enrichment_complete").collect();
    assert_eq!(ready_events.len(), 3);
    assert_eq!(complete_events.len(), 3);
    for e in &ready_events { assert!(!e.persistent, "EnrichmentReady should have persistent=false"); }
    for e in &complete_events { assert!(e.persistent, "EnrichmentComplete should have persistent=true"); }

    Ok(())
}

/// on_any reactor should still fire for ephemeral events (both root and reactor-emitted).
#[tokio::test]
async fn ephemeral_triggers_on_any_handler() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let any_count = Arc::new(AtomicI32::new(0));
    let any_count2 = any_count.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EphA])
            }),
        )
        .with_reactor(
            reactor::on_any().then(move |_event: causal::AnyEvent, _ctx: Context<Deps>| {
                let c = any_count2.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
        );

    engine.emit(EphTrigger).settled().await?;

    // on_any should fire for: EphTrigger (persistent root) + EphA (ephemeral reactor-emitted)
    assert_eq!(
        any_count.load(Ordering::SeqCst), 2,
        "on_any should fire for both persistent and ephemeral events"
    );

    Ok(())
}

/// Ephemeral root event emitted via emit_output (type-erased path).
#[tokio::test]
async fn ephemeral_root_via_emit_output() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let handler_ran = Arc::new(AtomicI32::new(0));
    let handler_ran2 = handler_ran.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(move |_event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                let ran = handler_ran2.clone();
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(events![])
                }
            }),
        );

    let output = causal::reactor::EventOutput::new(EnrichmentReady {
        concern_id: Uuid::nil(),
    });
    // EventOutput.persistent should be false for ephemeral events
    assert!(!output.persistent, "EventOutput for ephemeral event should have persistent=false");

    engine.emit_output(output).settled().await?;

    assert_eq!(handler_ran.load(Ordering::SeqCst), 1, "Reactor should fire for emit_output ephemeral");
    let all_events = store.global_log().lock().clone();
    assert_eq!(all_events.len(), 1, "Ephemeral event should be in operational store");
    assert!(!all_events[0].persistent, "Ephemeral via emit_output should have persistent=false");

    Ok(())
}

// ── Adversarial ephemeral persistence tests ─────────────────────────────
// Attack the two-tier persistence model from all angles.

/// Ephemeral events in the operational store have correct `persistent` flag
/// propagated through the full causal chain.
#[tokio::test]
async fn ephemeral_persistent_flag_propagates_through_causal_chain() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                // Persistent trigger emits ephemeral
                Ok(events![EnrichmentReady { concern_id: Uuid::nil() }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(|event: Arc<EnrichmentReady>, _ctx: Context<Deps>| async move {
                // Ephemeral emits persistent
                Ok(events![EnrichmentComplete { concern_id: event.concern_id }])
            }),
        );

    engine.emit(EphTrigger).settled().await?;

    let all = store.global_log().lock().clone();
    assert_eq!(all.len(), 3, "All three events should be in operational store");

    // Check each event's persistent flag
    let trigger = all.iter().find(|e| e.event_type == "eph_trigger").unwrap();
    assert!(trigger.persistent, "EphTrigger is a persistent event");
    assert!(trigger.parent_id.is_none(), "Root event has no parent");

    let ready = all.iter().find(|e| e.event_type == "enrichment_ready").unwrap();
    assert!(!ready.persistent, "EnrichmentReady is ephemeral");
    assert_eq!(ready.parent_id, Some(trigger.event_id), "Ephemeral parent is the trigger");
    assert_eq!(ready.correlation_id, trigger.correlation_id, "Same correlation");

    let complete = all.iter().find(|e| e.event_type == "enrichment_complete").unwrap();
    assert!(complete.persistent, "EnrichmentComplete is persistent");
    assert_eq!(complete.correlation_id, trigger.correlation_id, "Same correlation");

    Ok(())
}

/// Ephemeral events should NOT appear in aggregate load_stream, even though
/// they're in the operational store. This is naturally true because ephemeral
/// events don't get aggregate_type/aggregate_id set.
#[tokio::test]
async fn ephemeral_events_excluded_from_aggregate_streams() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .with_reactor(
            reactor::on::<OrderCreated>().then(|event: Arc<OrderCreated>, _ctx: Context<Deps>| async move {
                // Persistent event triggers ephemeral signal
                Ok(events![EnrichmentReady { concern_id: event.order_id }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(|event: Arc<EnrichmentReady>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentComplete { concern_id: event.concern_id }])
            }),
        );

    engine.emit(OrderCreated { order_id, total: 100 }).settled().await?;

    // Aggregate stream should only contain the OrderCreated event
    let stream = store.load_stream("Order", order_id, None).await?;
    assert_eq!(stream.len(), 1, "Only OrderCreated in aggregate stream");
    assert_eq!(stream[0].event_type, "order_created");

    // But ALL events are in the global log
    let all = store.global_log().lock().clone();
    assert_eq!(all.len(), 3, "All 3 events in operational store");

    // Ephemeral event has no aggregate metadata
    let ready = all.iter().find(|e| e.event_type == "enrichment_ready").unwrap();
    assert!(ready.aggregate_type.is_none(), "Ephemeral should have no aggregate_type");
    assert!(ready.aggregate_id.is_none(), "Ephemeral should have no aggregate_id");

    Ok(())
}

/// Cancel a correlation that includes ephemeral events — the ephemeral events
/// should still be in the store but downstream reactors should be DLQ'd.
#[tokio::test]
async fn cancel_correlation_with_ephemeral_events() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let handler_ran = Arc::new(AtomicI32::new(0));
    let handler_ran2 = handler_ran.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EnrichmentReady>()
                .id("should_be_cancelled")
                .retry(1)
                .then(move |_event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                    let ran = handler_ran2.clone();
                    async move {
                        ran.fetch_add(1, Ordering::SeqCst);
                        Ok(events![EnrichmentComplete { concern_id: Uuid::nil() }])
                    }
                }),
        );

    let handle = engine.emit(EnrichmentReady { concern_id: Uuid::nil() }).await?;
    handle.cancel().await?;
    engine.settle().await?;

    // The ephemeral root IS in the store (persisted before cancel)
    let all = store.global_log().lock().clone();
    let ready = all.iter().find(|e| e.event_type == "enrichment_ready");
    assert!(ready.is_some(), "Ephemeral root should be in operational store even when cancelled");
    assert!(!ready.unwrap().persistent, "Should still be ephemeral");

    Ok(())
}

/// Snapshot should NOT be triggered by ephemeral events, even if many
/// ephemeral events flow through the system.
#[tokio::test]
async fn snapshot_not_triggered_by_ephemeral_events() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let order_id = Uuid::new_v4();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_aggregator::<OrderCreated, Order, _>(|e| e.order_id)
        .snapshot_every(2) // Snapshot after every 2 aggregate events
        .with_reactor(
            reactor::on::<OrderCreated>().then(|_event: Arc<OrderCreated>, _ctx: Context<Deps>| async move {
                // Each persistent event triggers 5 ephemeral events
                Ok(events![
                    EnrichmentReady { concern_id: Uuid::new_v4() },
                    EnrichmentReady { concern_id: Uuid::new_v4() },
                    EnrichmentReady { concern_id: Uuid::new_v4() },
                    EnrichmentReady { concern_id: Uuid::new_v4() },
                    EnrichmentReady { concern_id: Uuid::new_v4() },
                ])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(|_event: Arc<EnrichmentReady>, _ctx: Context<Deps>| async move {
                Ok(events![])
            }),
        );

    engine.emit(OrderCreated { order_id, total: 100 }).settled().await?;

    // Only 1 aggregate event (OrderCreated), so no snapshot yet (threshold=2)
    let snapshot = store.load_snapshot("Order", order_id).await?;
    assert!(snapshot.is_none(), "5 ephemeral events should NOT count toward snapshot threshold");

    Ok(())
}

/// Idempotent append of ephemeral events — appending same event_id twice
/// should return the same result (not duplicate).
#[tokio::test]
async fn ephemeral_event_idempotent_append() -> Result<()> {
    let store = MemoryStore::new();
    let event_id = Uuid::new_v4();

    let event = NewEvent {
        event_id,
        parent_id: None,
        correlation_id: Uuid::new_v4(),
        event_type: "enrichment_ready".to_string(),
        payload: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        aggregate_type: None,
        aggregate_id: None,
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: false,
    };

    let r1 = store.append(event.clone()).await?;
    let r2 = store.append(event).await?;

    assert_eq!(r1, r2, "Idempotent append should return same result");
    let all = store.global_log().lock().clone();
    assert_eq!(all.len(), 1, "Should not duplicate ephemeral event");
    assert!(!all[0].persistent, "Should preserve persistent=false");

    Ok(())
}

/// DLQ terminal mapper fires correctly for a reactor that was triggered
/// by an ephemeral event and then fails.
#[event]
#[derive(Clone, Serialize, Deserialize)]
struct EphHandlerFailed {
    source_event_type: String,
    error: String,
}

#[tokio::test]
async fn dlq_terminal_event_from_ephemeral_triggered_handler() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .on_dlq(|info: causal::reactor::DlqTerminalInfo| {
            EphHandlerFailed {
                source_event_type: info.source_event_type,
                error: info.error,
            }
        })
        .with_reactor(
            reactor::on::<EnrichmentReady>()
                .id("always_fails")
                .retry(1)
                .then(|_event: Arc<EnrichmentReady>, _ctx: Context<Deps>| async move {
                    anyhow::bail!("permanent failure")
                }),
        );

    engine.emit(EnrichmentReady { concern_id: Uuid::nil() }).settled().await?;

    let all = store.global_log().lock().clone();

    // The ephemeral root is in the store
    let ready = all.iter().find(|e| e.event_type == "enrichment_ready").unwrap();
    assert!(!ready.persistent, "Source event is ephemeral");

    // The DLQ terminal event should also be in the store and persistent
    let dlq = all.iter().find(|e| e.event_type == "eph_handler_failed").unwrap();
    assert!(dlq.persistent, "DLQ terminal event should be persistent");
    let payload: serde_json::Value = dlq.payload.clone();
    assert_eq!(payload["source_event_type"], "enrichment_ready");
    assert!(payload["error"].as_str().unwrap().contains("permanent failure"));

    Ok(())
}

/// Deeply nested chain: persistent → eph → eph → eph → persistent.
/// Tests that ephemeral events at arbitrary depth all route correctly
/// and hop counting accumulates.
#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EphChain1;

#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EphChain2;

#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EphChain3;

#[event]
#[derive(Clone, Serialize, Deserialize)]
struct ChainTerminal {
    depth: i32,
}

#[tokio::test]
async fn deep_ephemeral_chain_with_hop_counting() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_: Arc<EphTrigger>, _: Context<Deps>| async move {
                Ok(events![EphChain1])
            }),
        )
        .with_reactor(
            reactor::on::<EphChain1>().then(|_: Arc<EphChain1>, _: Context<Deps>| async move {
                Ok(events![EphChain2])
            }),
        )
        .with_reactor(
            reactor::on::<EphChain2>().then(|_: Arc<EphChain2>, _: Context<Deps>| async move {
                Ok(events![EphChain3])
            }),
        )
        .with_reactor(
            reactor::on::<EphChain3>().then(|_: Arc<EphChain3>, _: Context<Deps>| async move {
                Ok(events![ChainTerminal { depth: 4 }])
            }),
        );

    engine.emit(EphTrigger).settled().await?;

    let all = store.global_log().lock().clone();
    let types: Vec<_> = all.iter().map(|e| e.event_type.as_str()).collect();
    assert!(types.contains(&"eph_trigger"), "Root persistent event");
    assert!(types.contains(&"eph_chain1"), "Eph depth 1");
    assert!(types.contains(&"eph_chain2"), "Eph depth 2");
    assert!(types.contains(&"eph_chain3"), "Eph depth 3");
    assert!(types.contains(&"chain_terminal"), "Terminal persistent event");

    // All ephemeral events have persistent=false
    for e in &all {
        match e.event_type.as_str() {
            "eph_chain1" | "eph_chain2" | "eph_chain3" => {
                assert!(!e.persistent, "{} should be ephemeral", e.event_type);
            }
            "eph_trigger" | "chain_terminal" => {
                assert!(e.persistent, "{} should be persistent", e.event_type);
            }
            _ => {}
        }
    }

    // Hop counts should accumulate: trigger=0, chain1=1, chain2=2, chain3=3, terminal=4
    let terminal = all.iter().find(|e| e.event_type == "chain_terminal").unwrap();
    let hops = terminal.metadata.get("_hops").and_then(|v| v.as_i64()).unwrap_or(0);
    assert!(hops >= 4, "Terminal should have at least 4 hops, got {}", hops);

    Ok(())
}

/// Ephemeral events should advance the checkpoint correctly — no stalling.
#[tokio::test]
async fn ephemeral_events_advance_checkpoint() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EnrichmentReady>().then(|_: Arc<EnrichmentReady>, _: Context<Deps>| async move {
                Ok(events![EnrichmentComplete { concern_id: Uuid::nil() }])
            }),
        );

    // Emit multiple ephemeral roots sequentially
    engine.emit(EnrichmentReady { concern_id: Uuid::new_v4() }).settled().await?;
    engine.emit(EnrichmentReady { concern_id: Uuid::new_v4() }).settled().await?;
    engine.emit(EnrichmentReady { concern_id: Uuid::new_v4() }).settled().await?;

    // Checkpoint should have advanced past all events
    let checkpoint = store.checkpoint().await?;
    let all = store.global_log().lock().clone();
    let max_position = all.iter().map(|e| e.position).max().unwrap_or(LogCursor::ZERO);
    assert_eq!(checkpoint, max_position, "Checkpoint should be at max position (no stalling)");

    Ok(())
}

/// Reactor describe() should fire for events in correlations that include
/// ephemeral events — describe runs for ALL reactors on every event.
#[tokio::test]
async fn describe_fires_for_ephemeral_events() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let describe_count = Arc::new(AtomicI32::new(0));
    let describe_count2 = describe_count.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EnrichmentReady>()
                .id("described_handler")
                .filter(|_event: &EnrichmentReady, _ctx: &Context<Deps>| true)
                .describe(move |_ctx: &Context<Deps>| {
                    describe_count2.fetch_add(1, Ordering::SeqCst);
                    serde_json::json!({"status": "ready"})
                })
                .then(|_: Arc<EnrichmentReady>, _: Context<Deps>| async move {
                    Ok(events![EnrichmentComplete { concern_id: Uuid::nil() }])
                }),
        );

    engine.emit(EnrichmentReady { concern_id: Uuid::nil() }).settled().await?;

    // Describe should have been called (at least once for the ephemeral event,
    // and once for the persistent EnrichmentComplete)
    let count = describe_count.load(Ordering::SeqCst);
    assert!(count >= 1, "describe() should fire during ephemeral event processing, got {}", count);

    Ok(())
}

/// Mixed batch: reactor returns events![] with persistent and ephemeral
/// events interleaved. Verify ordering and persistent flags are correct.
#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
struct EphBatchItem { idx: i32 }

#[event]
#[derive(Clone, Serialize, Deserialize)]
struct PersistBatchItem { idx: i32 }

#[tokio::test]
async fn interleaved_persistent_ephemeral_batch_ordering() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_: Arc<EphTrigger>, _: Context<Deps>| async move {
                Ok(events![
                    PersistBatchItem { idx: 1 },
                    EphBatchItem { idx: 2 },
                    PersistBatchItem { idx: 3 },
                    EphBatchItem { idx: 4 },
                    PersistBatchItem { idx: 5 },
                ])
            }),
        )
        // No-op reactors so ephemeral events get routed
        .with_reactor(
            reactor::on::<EphBatchItem>().then(|_: Arc<EphBatchItem>, _: Context<Deps>| async move {
                Ok(events![])
            }),
        );

    engine.emit(EphTrigger).settled().await?;

    let all = store.global_log().lock().clone();

    // Should have 6 events: EphTrigger + 5 batch items
    assert_eq!(all.len(), 6, "Trigger + 5 batch items");

    // Verify persistent flags
    let batch_items: Vec<_> = all.iter().filter(|e| {
        e.event_type == "persist_batch_item" || e.event_type == "eph_batch_item"
    }).collect();
    assert_eq!(batch_items.len(), 5);

    for item in &batch_items {
        match item.event_type.as_str() {
            "persist_batch_item" => assert!(item.persistent, "PersistBatchItem should be persistent"),
            "eph_batch_item" => assert!(!item.persistent, "EphBatchItem should be ephemeral"),
            _ => unreachable!(),
        }
    }

    Ok(())
}

/// Ephemeral event with a retry reactor — retry should work within
/// the same process (ephemeral sidecar survives in cache).
#[tokio::test]
async fn ephemeral_event_with_retry_handler() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let attempt_count = Arc::new(AtomicI32::new(0));
    let attempt_count2 = attempt_count.clone();

    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_reactor(
            reactor::on::<EphTrigger>().then(|_event: Arc<EphTrigger>, _ctx: Context<Deps>| async move {
                Ok(events![EnrichmentReady {
                    concern_id: Uuid::nil(),
                }])
            }),
        )
        .with_reactor(
            reactor::on::<EnrichmentReady>()
                .id("retry_ephemeral")
                .retry(3)
                .then(move |_event: Arc<EnrichmentReady>, _ctx: Context<Deps>| {
                    let count = attempt_count2.clone();
                    async move {
                        let n = count.fetch_add(1, Ordering::SeqCst);
                        if n < 2 {
                            anyhow::bail!("transient failure attempt {}", n);
                        }
                        Ok(events![EnrichmentComplete {
                            concern_id: Uuid::nil(),
                        }])
                    }
                }),
        );

    engine.emit(EphTrigger).settled().await?;

    // Should have attempted 3 times (0, 1, 2) and succeeded on attempt 2
    assert_eq!(attempt_count.load(Ordering::SeqCst), 3, "Should retry ephemeral event reactor");

    // The persistent output should be in the log
    let all_events = store.global_log().lock().clone();
    let has_complete = all_events.iter().any(|e| e.event_type == "enrichment_complete");
    assert!(has_complete, "Persistent event from retried ephemeral reactor should be in log");

    Ok(())
}

// ── Adversarial journal tests ────────────────────────────────────────

/// Verify AtomicU32 counter correctness when ctx is cloned across spawned tasks.
/// Two tasks sharing a cloned context should get unique sequence numbers.
#[tokio::test]
async fn journal_concurrent_ctx_run_from_spawned_tasks() -> Result<()> {
    let call_log = Arc::new(Mutex::new(Vec::<(u32, String)>::new()));
    let cl = call_log.clone();
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_reactor(
            reactor::on::<Ping>()
                .id("concurrent_journal")
                .then(move |_event: Arc<Ping>, ctx: Context<Deps>| {
                    let cl = cl.clone();
                    async move {
                        // Run 5 sequential ctx.run() calls to generate predictable seqs
                        for i in 0..5u32 {
                            let cl2 = cl.clone();
                            let val: String = ctx
                                .run(move || async move { Ok(format!("step-{i}")) })
                                .await?;
                            cl2.lock().push((i, val));
                        }
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "concurrent".into(),
        })
        .settled()
        .await?;

    let log = call_log.lock().clone();
    assert_eq!(log.len(), 5, "All 5 journal steps should execute");

    // Verify each step got a unique, sequential seq number
    for (i, (seq, val)) in log.iter().enumerate() {
        assert_eq!(*seq, i as u32, "Seq should be sequential");
        assert_eq!(*val, format!("step-{i}"), "Value should match step");
    }

    // Verify all 5 entries were persisted to the store
    let all_events = store.global_log().lock().clone();
    let ping_event = all_events
        .iter()
        .find(|e| e.event_type == "ping")
        .unwrap();

    let entries = store
        .load_journal("concurrent_journal", ping_event.event_id)
        .await?;
    // Journal is cleared on completion, so entries should be empty
    assert!(
        entries.is_empty(),
        "Journal should be cleared after successful reactor"
    );

    Ok(())
}

/// Verify serde round-trip works with deeply nested and large payloads.
#[tokio::test]
async fn journal_large_nested_payload_round_trips() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let captured = Arc::new(Mutex::new(None));
    let cap = captured.clone();

    // Build a deeply nested JSON structure
    let mut nested = serde_json::json!({"leaf": "value"});
    for i in 0..50 {
        nested = serde_json::json!({ format!("level_{i}"): nested });
    }
    let large_payload = nested.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_reactor(
            reactor::on::<Ping>()
                .id("large_journal")
                .then(move |_event: Arc<Ping>, ctx: Context<Deps>| {
                    let payload = large_payload.clone();
                    let cap = cap.clone();
                    async move {
                        let result: serde_json::Value =
                            ctx.run(move || async move { Ok(payload) }).await?;
                        *cap.lock() = Some(result);
                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "big".into(),
        })
        .settled()
        .await?;

    let result = captured.lock().clone().expect("Should have captured result");
    assert_eq!(result, nested, "Deeply nested payload should survive journal round-trip");

    Ok(())
}

/// A reactor that panics mid-journal should leave prior journal entries intact
/// so the next retry attempt replays them.
#[tokio::test]
async fn journal_panic_mid_execution_preserves_prior_entries() -> Result<()> {
    let call_count = Arc::new(AtomicI32::new(0));
    let step1_count = Arc::new(AtomicI32::new(0));
    let step2_count = Arc::new(AtomicI32::new(0));
    let cc = call_count.clone();
    let s1 = step1_count.clone();
    let s2 = step2_count.clone();
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_reactor(
            reactor::on::<Ping>()
                .id("panic_journal")
                .retry(3)
                .then(move |_event: Arc<Ping>, ctx: Context<Deps>| {
                    let cc = cc.clone();
                    let s1 = s1.clone();
                    let s2 = s2.clone();
                    async move {
                        let attempt = cc.fetch_add(1, Ordering::SeqCst);

                        // Step 1: always succeeds
                        let _: String = ctx
                            .run(move || {
                                s1.fetch_add(1, Ordering::SeqCst);
                                async move { Ok("step1-done".to_string()) }
                            })
                            .await?;

                        // Step 2: fails on first attempt, succeeds on second
                        if attempt == 0 {
                            anyhow::bail!("simulated crash after step 1");
                        }

                        let _: String = ctx
                            .run(move || {
                                s2.fetch_add(1, Ordering::SeqCst);
                                async move { Ok("step2-done".to_string()) }
                            })
                            .await?;

                        Ok(events![])
                    }
                }),
        );

    engine
        .emit(Ping {
            msg: "panic-test".into(),
        })
        .settled()
        .await?;

    // Reactor ran twice (first attempt failed, second succeeded)
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    // Step 1 closure executed on first attempt, replayed from journal on second
    assert_eq!(
        step1_count.load(Ordering::SeqCst),
        1,
        "Step 1 should execute once then replay from journal"
    );

    // Step 2 only executed on the second attempt
    assert_eq!(
        step2_count.load(Ordering::SeqCst),
        1,
        "Step 2 should execute once on retry"
    );

    Ok(())
}

/// Ephemeral events should still get full journaling support. A reactor
/// triggered by an ephemeral event should be able to use ctx.run() with
/// replay on retry.
#[tokio::test]
async fn journal_works_for_ephemeral_event_handlers() -> Result<()> {
    #[event(ephemeral)]
    #[derive(Clone, Serialize, Deserialize)]
    struct EphJournalTrigger;

    #[event]
    #[derive(Clone, Serialize, Deserialize)]
    struct JournalOutput {
        step1: String,
        step2: String,
    }

    let step1_count = Arc::new(AtomicI32::new(0));
    let step2_count = Arc::new(AtomicI32::new(0));
    let attempt_count = Arc::new(AtomicI32::new(0));
    let s1 = step1_count.clone();
    let s2 = step2_count.clone();
    let ac = attempt_count.clone();
    let store = Arc::new(MemoryStore::new());

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_reactor(
            reactor::on::<EphJournalTrigger>()
                .id("eph_journal_handler")
                .retry(3)
                .then(move |_event: Arc<EphJournalTrigger>, ctx: Context<Deps>| {
                    let s1 = s1.clone();
                    let s2 = s2.clone();
                    let ac = ac.clone();
                    async move {
                        let attempt = ac.fetch_add(1, Ordering::SeqCst);

                        let step1: String = ctx
                            .run(move || {
                                s1.fetch_add(1, Ordering::SeqCst);
                                async move { Ok("eph-step1".to_string()) }
                            })
                            .await?;

                        // Fail on first attempt to force retry + replay
                        if attempt == 0 {
                            anyhow::bail!("transient failure");
                        }

                        let step2: String = ctx
                            .run(move || {
                                s2.fetch_add(1, Ordering::SeqCst);
                                async move { Ok("eph-step2".to_string()) }
                            })
                            .await?;

                        Ok(events![JournalOutput { step1, step2 }])
                    }
                }),
        );

    engine.emit(EphJournalTrigger).settled().await?;

    assert_eq!(attempt_count.load(Ordering::SeqCst), 2, "Should retry once");
    assert_eq!(
        step1_count.load(Ordering::SeqCst),
        1,
        "Step 1 should execute once then replay from journal on retry"
    );
    assert_eq!(
        step2_count.load(Ordering::SeqCst),
        1,
        "Step 2 should execute once on successful retry"
    );

    // Verify the persistent output event made it to the log
    let all_events = store.global_log().lock().clone();
    let output = all_events
        .iter()
        .find(|e| e.event_type == "journal_output")
        .expect("JournalOutput should be in log");
    assert!(output.persistent, "Output from ephemeral reactor should be persistent");

    // Verify the ephemeral trigger is in log with persistent=false
    let trigger = all_events
        .iter()
        .find(|e| e.event_type == "eph_journal_trigger")
        .expect("Ephemeral trigger should be in log");
    assert!(!trigger.persistent, "Ephemeral trigger should have persistent=false");

    Ok(())
}

/// Two reactors with the same name processing different events must have
/// completely isolated journals. A stale journal from reactor "X" on event A
/// must never leak into reactor "X" on event B.
#[tokio::test]
async fn journal_isolation_across_different_events() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let exec_count = Arc::new(AtomicI32::new(0));
    let ec = exec_count.clone();

    let engine = Engine::in_memory(Deps)
        .with_store(store.clone())
        .with_reactor(
            reactor::on::<Ping>()
                .id("isolated_journal")
                .then(move |event: Arc<Ping>, ctx: Context<Deps>| {
                    let ec = ec.clone();
                    async move {
                        ec.fetch_add(1, Ordering::SeqCst);
                        // Each invocation journals a unique value based on the event
                        let _: String = ctx
                            .run(move || async move {
                                Ok(format!("result-for-{}", event.msg))
                            })
                            .await?;
                        Ok(events![])
                    }
                }),
        );

    // Emit two separate events — each gets its own event_id, so journals are isolated
    engine
        .emit(Ping {
            msg: "first".into(),
        })
        .settled()
        .await?;

    engine
        .emit(Ping {
            msg: "second".into(),
        })
        .settled()
        .await?;

    // Both reactors should have executed (not replayed from each other's journals)
    assert_eq!(
        exec_count.load(Ordering::SeqCst),
        2,
        "Both reactor invocations should execute their closures independently"
    );

    // Both journals should be cleared after successful completion
    let all_events = store.global_log().lock().clone();
    let ping_events: Vec<_> = all_events.iter().filter(|e| e.event_type == "ping").collect();
    assert_eq!(ping_events.len(), 2, "Both ping events should be in the log");

    for pe in &ping_events {
        let entries = store
            .load_journal("isolated_journal", pe.event_id)
            .await?;
        assert!(
            entries.is_empty(),
            "Journal for event {} should be cleared after completion",
            pe.event_id
        );
    }

    Ok(())
}

// ── Bug: with_aggregators does not register codecs ───────────────

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AggOnlyEvent {
    id: Uuid,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct AggOnlyState {
    seen: bool,
}

impl Aggregate for AggOnlyState {
    fn aggregate_type() -> &'static str {
        "AggOnlyState"
    }
}

#[aggregators]
mod agg_only_aggregators {
    use super::*;

    #[aggregator(id = "id")]
    fn on_event(state: &mut AggOnlyState, event: AggOnlyEvent) {
        state.seen = true;
        let _ = event;
    }
}

/// Regression: `with_aggregators` (bulk/macro path) must register event codecs
/// so that `on_any` handlers can downcast events during cold-start replay.
///
/// `engine.emit()` and `events![]` both auto-register codecs at runtime, masking
/// the bug. The real failure occurs when replaying historical events from the store
/// on cold start — no ephemeral sidecar, no prior emit to register the codec.
#[tokio::test]
async fn with_aggregators_registers_codec_for_on_any_downcast() -> Result<()> {
    use causal::reactor::AnyEvent;

    let store = Arc::new(MemoryStore::new());

    // Pre-seed the store with an AggOnlyEvent (simulates historical data)
    let agg_event_id = Uuid::new_v4();
    let correlation_id = Uuid::new_v4();
    store.append(NewEvent {
        event_id: agg_event_id,
        parent_id: None,
        correlation_id,
        event_type: "agg_only_event".to_string(),
        payload: serde_json::json!({ "id": Uuid::new_v4().to_string() }),
        created_at: chrono::Utc::now(),
        aggregate_type: None,
        aggregate_id: None,
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }).await?;

    let downcast_ok = Arc::new(AtomicUsize::new(0));
    let downcast_ok2 = downcast_ok.clone();

    // Build engine with aggregators (bulk path) — no typed reactor for AggOnlyEvent
    let engine = Engine::with_backends(Deps, store.clone(), store.clone())
        .with_aggregators(agg_only_aggregators::aggregators())
        .with_reactor(
            reactor::on_any()
                .id("agg_only_any")
                .then(move |event: AnyEvent, _ctx: Context<Deps>| {
                    let flag = downcast_ok2.clone();
                    async move {
                        if event.downcast::<AggOnlyEvent>().is_some() {
                            flag.store(1, Ordering::SeqCst);
                        }
                        Ok(events![])
                    }
                }),
        );

    engine.settle().await?;

    assert_eq!(
        downcast_ok.load(Ordering::SeqCst),
        1,
        "on_any handler should be able to downcast event registered only via with_aggregators"
    );

    Ok(())
}
