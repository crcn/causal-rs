use anyhow::Result;
use seesaw_core::es::*;
use seesaw_memory::event_store::MemoryEventStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Test domain ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OrderStatus {
    #[default]
    Draft,
    Placed,
    Shipped,
    Cancelled,
}

#[derive(Default)]
struct Order {
    status: OrderStatus,
    total: f64,
    items: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    ItemAdded { order_id: Uuid },
    Shipped { order_id: Uuid },
    Cancelled { order_id: Uuid, reason: String },
}

impl EventUpcast for OrderEvent {}

impl Aggregate for Order {
    type Event = OrderEvent;

    fn aggregate_type() -> &'static str {
        "Order"
    }

    fn apply(&mut self, event: OrderEvent) {
        match event {
            OrderEvent::Placed { total, .. } => {
                self.status = OrderStatus::Placed;
                self.total = total;
            }
            OrderEvent::ItemAdded { .. } => {
                self.items += 1;
            }
            OrderEvent::Shipped { .. } => {
                self.status = OrderStatus::Shipped;
            }
            OrderEvent::Cancelled { .. } => {
                self.status = OrderStatus::Cancelled;
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_append_load() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    let events = vec![
        OrderEvent::Placed {
            order_id: id,
            total: 99.99,
        },
        OrderEvent::ItemAdded { order_id: id },
    ];

    let version = store
        .aggregate::<Order>(id)
        .append(0, events)
        .await?;

    assert_eq!(version, 2);

    let order: Versioned<Order> = store.aggregate(id).load().await?;
    assert_eq!(order.version(), 2);
    assert_eq!(order.status, OrderStatus::Placed);
    assert!((order.total - 99.99).abs() < f64::EPSILON);
    assert_eq!(order.items, 1);

    Ok(())
}

#[tokio::test]
async fn optimistic_concurrency_conflict() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    // First append succeeds
    store
        .aggregate::<Order>(id)
        .append(
            0,
            vec![OrderEvent::Placed {
                order_id: id,
                total: 50.0,
            }],
        )
        .await?;

    // Second append with stale version fails
    let result = store
        .aggregate::<Order>(id)
        .append(
            0, // stale!
            vec![OrderEvent::Shipped { order_id: id }],
        )
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.downcast_ref::<ConcurrencyError>().is_some());

    let conflict = err.downcast_ref::<ConcurrencyError>().unwrap();
    assert_eq!(conflict.expected, 0);
    assert_eq!(conflict.actual, 1);

    Ok(())
}

#[tokio::test]
async fn empty_aggregate_loads_default() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    let order: Versioned<Order> = store.aggregate(id).load().await?;
    assert_eq!(order.version(), 0);
    assert_eq!(order.status, OrderStatus::Draft);
    assert!((order.total - 0.0).abs() < f64::EPSILON);

    Ok(())
}

#[tokio::test]
async fn event_ordering_preserved() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    store
        .aggregate::<Order>(id)
        .append(
            0,
            vec![OrderEvent::Placed {
                order_id: id,
                total: 10.0,
            }],
        )
        .await?;

    store
        .aggregate::<Order>(id)
        .append(
            1,
            vec![
                OrderEvent::ItemAdded { order_id: id },
                OrderEvent::ItemAdded { order_id: id },
                OrderEvent::ItemAdded { order_id: id },
            ],
        )
        .await?;

    let order: Versioned<Order> = store.aggregate(id).load().await?;
    assert_eq!(order.version(), 4);
    assert_eq!(order.items, 3);
    assert_eq!(order.status, OrderStatus::Placed);

    Ok(())
}

#[tokio::test]
async fn version_tracking() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    let v1 = store
        .aggregate::<Order>(id)
        .append(
            0,
            vec![OrderEvent::Placed {
                order_id: id,
                total: 100.0,
            }],
        )
        .await?;
    assert_eq!(v1, 1);

    let v2 = store
        .aggregate::<Order>(id)
        .append(v1, vec![OrderEvent::Shipped { order_id: id }])
        .await?;
    assert_eq!(v2, 2);

    let order: Versioned<Order> = store.aggregate(id).load().await?;
    assert_eq!(order.version(), 2);
    assert_eq!(order.status, OrderStatus::Shipped);

    Ok(())
}

#[tokio::test]
async fn caused_by_preserved() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();
    let cause_id = Uuid::new_v4();

    store
        .aggregate::<Order>(id)
        .append_caused_by(
            0,
            cause_id,
            vec![OrderEvent::Placed {
                order_id: id,
                total: 42.0,
            }],
        )
        .await?;

    let events = store.load_events(id, 0).await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].caused_by, Some(cause_id));

    Ok(())
}

#[tokio::test]
async fn causal_chain_across_appends() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    // First event — no cause
    store
        .aggregate::<Order>(id)
        .append(
            0,
            vec![OrderEvent::Placed {
                order_id: id,
                total: 10.0,
            }],
        )
        .await?;

    let events = store.load_events(id, 0).await?;
    let placed_event_id = events[0].id;

    // Second event caused by the first
    store
        .aggregate::<Order>(id)
        .append_caused_by(
            1,
            placed_event_id,
            vec![OrderEvent::Shipped { order_id: id }],
        )
        .await?;

    let all_events = store.load_events(id, 0).await?;
    assert_eq!(all_events.len(), 2);
    assert_eq!(all_events[0].caused_by, None);
    assert_eq!(all_events[1].caused_by, Some(placed_event_id));

    Ok(())
}

#[tokio::test]
async fn load_at_version() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    store
        .aggregate::<Order>(id)
        .append(
            0,
            vec![
                OrderEvent::Placed {
                    order_id: id,
                    total: 100.0,
                },
                OrderEvent::Shipped { order_id: id },
            ],
        )
        .await?;

    // Load at version 1 (only Placed)
    let prev: Versioned<Order> = store.aggregate(id).load_at_version(1).await?;
    assert_eq!(prev.version(), 1);
    assert_eq!(prev.status, OrderStatus::Placed);

    // Load at version 2 (Placed + Shipped)
    let next: Versioned<Order> = store.aggregate(id).load_at_version(2).await?;
    assert_eq!(next.version(), 2);
    assert_eq!(next.status, OrderStatus::Shipped);

    Ok(())
}

#[tokio::test]
async fn exists_check() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    assert!(!store.exists(id).await?);

    store
        .aggregate::<Order>(id)
        .append(
            0,
            vec![OrderEvent::Placed {
                order_id: id,
                total: 1.0,
            }],
        )
        .await?;

    assert!(store.exists(id).await?);

    Ok(())
}

#[tokio::test]
async fn global_position_monotonically_increasing() -> Result<()> {
    let store = MemoryEventStore::new();

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    store
        .aggregate::<Order>(id1)
        .append(
            0,
            vec![OrderEvent::Placed {
                order_id: id1,
                total: 10.0,
            }],
        )
        .await?;

    store
        .aggregate::<Order>(id2)
        .append(
            0,
            vec![
                OrderEvent::Placed {
                    order_id: id2,
                    total: 20.0,
                },
                OrderEvent::ItemAdded { order_id: id2 },
            ],
        )
        .await?;

    let events1 = store.load_events(id1, 0).await?;
    let events2 = store.load_events(id2, 0).await?;

    // All positions should be unique and increasing
    let mut all_positions: Vec<u64> = events1
        .iter()
        .chain(events2.iter())
        .map(|e| e.position)
        .collect();
    all_positions.sort();

    assert_eq!(all_positions, vec![1, 2, 3]);

    Ok(())
}

// ── EventUpcast tests ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaymentEventV2 {
    order_id: Uuid,
    amount: f64,
    currency: String,
}

impl EventUpcast for PaymentEventV2 {
    fn current_version() -> u32 {
        2
    }

    fn upcast_from(version: u32, data: serde_json::Value) -> anyhow::Result<Self> {
        match version {
            1 => {
                // V1 didn't have currency
                let order_id: Uuid =
                    serde_json::from_value(data["order_id"].clone())?;
                let amount: f64 = serde_json::from_value(data["amount"].clone())?;
                Ok(PaymentEventV2 {
                    order_id,
                    amount,
                    currency: "USD".to_string(),
                })
            }
            2 => Ok(serde_json::from_value(data)?),
            _ => anyhow::bail!("unknown version {version}"),
        }
    }
}

#[derive(Default)]
struct Payment {
    total: f64,
    currency: String,
}

impl Aggregate for Payment {
    type Event = PaymentEventV2;

    fn aggregate_type() -> &'static str {
        "Payment"
    }

    fn apply(&mut self, event: PaymentEventV2) {
        self.total += event.amount;
        self.currency = event.currency;
    }
}

#[tokio::test]
async fn event_upcast_v1_to_v2() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    // Simulate a V1 event (no currency field) stored directly
    let v1_data = serde_json::json!({
        "order_id": id,
        "amount": 50.0
    });

    let new_event = NewEvent {
        event_type: "PaymentEventV2".to_string(),
        data: v1_data,
        metadata: None,
        schema_version: 1, // stored as V1
        caused_by: None,
    };

    store
        .append(id, "Payment", 0, vec![new_event])
        .await?;

    // Load should upcast V1 → V2 (adding currency = "USD")
    let payment: Versioned<Payment> = store.aggregate(id).load().await?;
    assert_eq!(payment.version(), 1);
    assert!((payment.total - 50.0).abs() < f64::EPSILON);
    assert_eq!(payment.currency, "USD");

    Ok(())
}

#[tokio::test]
async fn empty_append_is_noop() -> Result<()> {
    let store = MemoryEventStore::new();
    let id = Uuid::new_v4();

    let version = store
        .aggregate::<Order>(id)
        .append(0, vec![])
        .await?;

    assert_eq!(version, 0);
    assert!(!store.exists(id).await?);

    Ok(())
}
