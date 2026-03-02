//! Persistent event store — global event log with optional aggregate metadata.
//!
//! Separate from `MemoryStore` (the settle-loop queue) — `EventStore` is the
//! persistent event log for replay, projections, and aggregate reconstruction.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use dashmap::DashMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::aggregator::{Aggregate, AggregatorRegistry};

// ── Types ──────────────────────────────────────────────────────────

/// A persisted event loaded from the store.
#[derive(Debug, Clone)]
pub struct PersistedEvent {
    /// Global ordering position.
    pub position: u64,
    /// Unique event ID.
    pub event_id: Uuid,
    /// Parent event that caused this event (None for root events).
    pub parent_id: Option<Uuid>,
    /// Correlation ID linking the full causal tree.
    pub correlation_id: Uuid,
    /// Short stable event type name (e.g. "OrderPlaced").
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// When the event was persisted.
    pub created_at: DateTime<Utc>,
    /// Aggregate type (only present for aggregate-scoped events).
    pub aggregate_type: Option<String>,
    /// Aggregate instance ID (only present for aggregate-scoped events).
    pub aggregate_id: Option<Uuid>,
    /// Per-aggregate stream version (only present for aggregate-scoped events).
    pub version: Option<u64>,
}

/// A new event to be appended to the global log.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// Unique event ID.
    pub event_id: Uuid,
    /// Parent event that caused this event (None for root events).
    pub parent_id: Option<Uuid>,
    /// Correlation ID linking the full causal tree.
    pub correlation_id: Uuid,
    /// Short stable event type name (e.g. "OrderPlaced").
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// When the event was created.
    pub created_at: DateTime<Utc>,
    /// Aggregate type (set by engine when aggregators match).
    pub aggregate_type: Option<String>,
    /// Aggregate instance ID (set by engine when aggregators match).
    pub aggregate_id: Option<Uuid>,
}

/// Wrapper that pairs aggregate state with its stream version.
#[derive(Debug, Clone)]
pub struct Versioned<A> {
    pub state: A,
    pub version: u64,
}

// ── Helpers ────────────────────────────────────────────────────────

/// Extract the short type name from a full Rust type path.
///
/// `"my_crate::events::OrderPlaced"` → `"OrderPlaced"`
pub fn event_type_short_name(full: &str) -> &str {
    full.rsplit("::").next().unwrap_or(full)
}

// ── EventStore trait ───────────────────────────────────────────────

/// Persistent event log for all events.
///
/// Every event is appended to the global log. Events with aggregate metadata
/// can be loaded as streams for aggregate hydration.
pub trait EventStore: Send + Sync {
    /// Append a single event to the global log. Returns global position.
    fn append(
        &self,
        event: NewEvent,
    ) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + '_>>;

    /// Load events for an aggregate stream (for hydration).
    fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>>;

    /// Load events from after a specific position (for snapshot + partial replay).
    fn load_stream_from(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_position: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>>;
}

// ── MemoryEventStore ───────────────────────────────────────────────

/// In-memory `EventStore` for testing.
pub struct MemoryEventStore {
    global_log: Mutex<Vec<PersistedEvent>>,
    global_position: AtomicU64,
}

impl MemoryEventStore {
    pub fn new() -> Self {
        Self {
            global_log: Mutex::new(Vec::new()),
            global_position: AtomicU64::new(1),
        }
    }
}

impl Default for MemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStore for MemoryEventStore {
    fn append(
        &self,
        event: NewEvent,
    ) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + '_>> {
        let mut log = self.global_log.lock().unwrap();
        let position = self.global_position.fetch_add(1, Ordering::SeqCst);

        // Compute per-aggregate version if aggregate metadata is present
        let version = if let (Some(ref agg_type), Some(agg_id)) = (&event.aggregate_type, event.aggregate_id) {
            let count = log.iter().filter(|e| {
                e.aggregate_type.as_deref() == Some(agg_type) && e.aggregate_id == Some(agg_id)
            }).count() as u64;
            Some(count + 1)
        } else {
            None
        };

        log.push(PersistedEvent {
            position,
            event_id: event.event_id,
            parent_id: event.parent_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type,
            payload: event.payload,
            created_at: event.created_at,
            aggregate_type: event.aggregate_type,
            aggregate_id: event.aggregate_id,
            version,
        });

        Box::pin(std::future::ready(Ok(position)))
    }

    fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>> {
        let log = self.global_log.lock().unwrap();
        let events: Vec<PersistedEvent> = log
            .iter()
            .filter(|e| {
                e.aggregate_type.as_deref() == Some(aggregate_type)
                    && e.aggregate_id == Some(aggregate_id)
            })
            .cloned()
            .collect();
        Box::pin(std::future::ready(Ok(events)))
    }

    fn load_stream_from(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_position: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>> {
        let log = self.global_log.lock().unwrap();
        let events: Vec<PersistedEvent> = log
            .iter()
            .filter(|e| {
                e.aggregate_type.as_deref() == Some(aggregate_type)
                    && e.aggregate_id == Some(aggregate_id)
                    && e.position > after_position
            })
            .cloned()
            .collect();
        Box::pin(std::future::ready(Ok(events)))
    }
}

// ── Standalone functions ───────────────────────────────────────────

/// Persist an event to the event store.
///
/// Uses the short type name (e.g. `"OrderPlaced"`) for durable storage.
/// Returns the global position after appending.
pub async fn persist_event<E, A>(
    store: &dyn EventStore,
    aggregate_id: Uuid,
    event: &E,
) -> Result<u64>
where
    E: serde::Serialize + 'static,
    A: Aggregate,
{
    let event_type = event_type_short_name(std::any::type_name::<E>()).to_string();
    let payload = serde_json::to_value(event)?;

    store
        .append(NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type,
            payload,
            created_at: Utc::now(),
            aggregate_type: Some(A::aggregate_type().to_string()),
            aggregate_id: Some(aggregate_id),
        })
        .await
}

// ── Snapshot types ──────────────────────────────────────────────────

/// A serialized snapshot of aggregate state at a specific stream version.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub version: u64,
    pub state: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Persistent store for aggregate snapshots.
///
/// Optional optimization — without it, cold-start hydration replays
/// all events from the EventStore.
pub trait SnapshotStore: Send + Sync {
    /// Load the latest snapshot for an aggregate.
    fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Snapshot>>> + Send + '_>>;

    /// Save a snapshot of aggregate state.
    fn save_snapshot(
        &self,
        snapshot: Snapshot,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

/// In-memory `SnapshotStore` for testing.
pub struct MemorySnapshotStore {
    snapshots: DashMap<(String, Uuid), Snapshot>,
}

impl MemorySnapshotStore {
    pub fn new() -> Self {
        Self {
            snapshots: DashMap::new(),
        }
    }
}

impl Default for MemorySnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore for MemorySnapshotStore {
    fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Snapshot>>> + Send + '_>> {
        let key = (aggregate_type.to_string(), aggregate_id);
        let snapshot = self.snapshots.get(&key).map(|v| v.value().clone());
        Box::pin(std::future::ready(Ok(snapshot)))
    }

    fn save_snapshot(
        &self,
        snapshot: Snapshot,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let key = (snapshot.aggregate_type.clone(), snapshot.aggregate_id);
        self.snapshots.insert(key, snapshot);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Save a snapshot of aggregate state from the AggregatorRegistry.
///
/// Serializes the current in-memory state of aggregate `A` at `id` and
/// persists it to the snapshot store for future hydration acceleration.
pub async fn save_snapshot<A: Aggregate + serde::Serialize + serde::de::DeserializeOwned>(
    snapshot_store: &dyn SnapshotStore,
    aggregators: &AggregatorRegistry,
    id: Uuid,
) -> Result<()> {
    let key = format!("{}:{}", A::aggregate_type(), id);
    let version = aggregators.get_version(&key);

    // Find the aggregator to serialize state
    let agg = aggregators
        .find_first_by_aggregate_type(A::aggregate_type())
        .ok_or_else(|| anyhow::anyhow!("No aggregator registered for {}", A::aggregate_type()))?;

    let state_ref = aggregators
        .get_state(&key)
        .ok_or_else(|| anyhow::anyhow!("No state for aggregate {}:{}", A::aggregate_type(), id))?;

    let state_json = agg.serialize_state(state_ref.as_ref())?;

    snapshot_store
        .save_snapshot(Snapshot {
            aggregate_type: A::aggregate_type().to_string(),
            aggregate_id: id,
            version,
            state: state_json,
            created_at: Utc::now(),
        })
        .await
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_short_name_extracts_last_segment() {
        assert_eq!(event_type_short_name("my_crate::events::OrderPlaced"), "OrderPlaced");
        assert_eq!(event_type_short_name("OrderPlaced"), "OrderPlaced");
        assert_eq!(event_type_short_name("a::b::c::Foo"), "Foo");
    }

    fn make_new_event(event_type: &str, payload: serde_json::Value) -> NewEvent {
        NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: event_type.to_string(),
            payload,
            created_at: Utc::now(),
            aggregate_type: None,
            aggregate_id: None,
        }
    }

    fn make_aggregate_event(
        event_type: &str,
        payload: serde_json::Value,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> NewEvent {
        NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: event_type.to_string(),
            payload,
            created_at: Utc::now(),
            aggregate_type: Some(aggregate_type.to_string()),
            aggregate_id: Some(aggregate_id),
        }
    }

    #[tokio::test]
    async fn empty_stream_returns_empty() {
        let store = MemoryEventStore::new();
        let events = store.load_stream("Order", Uuid::new_v4()).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn append_and_load_aggregate_events() {
        let store = MemoryEventStore::new();
        let id = Uuid::new_v4();

        store.append(make_aggregate_event("OrderPlaced", serde_json::json!({"total": 100}), "Order", id)).await.unwrap();
        store.append(make_aggregate_event("OrderShipped", serde_json::json!({"tracking": "ABC"}), "Order", id)).await.unwrap();

        let events = store.load_stream("Order", id).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].version, Some(1));
        assert_eq!(events[0].event_type, "OrderPlaced");
        assert_eq!(events[1].version, Some(2));
        assert_eq!(events[1].event_type, "OrderShipped");
        assert_eq!(events[0].aggregate_type.as_deref(), Some("Order"));
    }

    #[tokio::test]
    async fn non_aggregate_event_is_persisted() {
        let store = MemoryEventStore::new();

        let pos = store.append(make_new_event("SystemStarted", serde_json::json!({"node": "a"}))).await.unwrap();
        assert!(pos > 0);

        // Not loadable via load_stream (no aggregate)
        let events = store.load_stream("System", Uuid::new_v4()).await.unwrap();
        assert!(events.is_empty());

        // But it's in the global log
        let log = store.global_log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].event_type, "SystemStarted");
        assert!(log[0].aggregate_type.is_none());
        assert!(log[0].version.is_none());
    }

    #[tokio::test]
    async fn load_stream_from_filters_by_position() {
        let store = MemoryEventStore::new();
        let id = Uuid::new_v4();

        let pos1 = store.append(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id)).await.unwrap();
        store.append(make_aggregate_event("OrderShipped", serde_json::json!({}), "Order", id)).await.unwrap();
        store.append(make_aggregate_event("OrderDelivered", serde_json::json!({}), "Order", id)).await.unwrap();

        let events = store.load_stream_from("Order", id, pos1).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "OrderShipped");
        assert_eq!(events[1].event_type, "OrderDelivered");
    }

    #[tokio::test]
    async fn streams_are_isolated() {
        let store = MemoryEventStore::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        store.append(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id1)).await.unwrap();
        store.append(make_aggregate_event("UserCreated", serde_json::json!({}), "User", id2)).await.unwrap();

        let events1 = store.load_stream("Order", id1).await.unwrap();
        let events2 = store.load_stream("User", id2).await.unwrap();
        assert_eq!(events1.len(), 1);
        assert_eq!(events2.len(), 1);
        assert_eq!(events1[0].event_type, "OrderPlaced");
        assert_eq!(events2[0].event_type, "UserCreated");
    }

    #[tokio::test]
    async fn position_increments_globally() {
        let store = MemoryEventStore::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        store.append(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id1)).await.unwrap();
        store.append(make_aggregate_event("UserCreated", serde_json::json!({}), "User", id2)).await.unwrap();

        let events1 = store.load_stream("Order", id1).await.unwrap();
        let events2 = store.load_stream("User", id2).await.unwrap();

        // Positions should be globally unique and ordered
        assert!(events2[0].position > events1[0].position);
    }

    #[tokio::test]
    async fn persist_event_appends_to_store() {
        use crate::aggregator::Aggregate;

        #[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
        struct Order;

        impl Aggregate for Order {
            fn aggregate_type() -> &'static str {
                "Order"
            }
        }

        #[derive(serde::Serialize)]
        struct OrderPlaced {
            total: u64,
        }

        let store = MemoryEventStore::new();
        let order_id = Uuid::new_v4();

        let position = persist_event::<OrderPlaced, Order>(
            &store,
            order_id,
            &OrderPlaced { total: 100 },
        )
        .await
        .unwrap();

        assert!(position > 0);

        let events = store.load_stream("Order", order_id).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "OrderPlaced");
        assert_eq!(events[0].aggregate_type.as_deref(), Some("Order"));
    }

    #[tokio::test]
    async fn causal_metadata_round_trips() {
        let store = MemoryEventStore::new();
        let event_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();

        store
            .append(NewEvent {
                event_id,
                parent_id: Some(parent_id),
                correlation_id,
                event_type: "TestEvent".to_string(),
                payload: serde_json::json!({}),
                created_at: Utc::now(),
                aggregate_type: Some("Test".to_string()),
                aggregate_id: Some(Uuid::new_v4()),
            })
            .await
            .unwrap();

        let log = store.global_log.lock().unwrap();
        assert_eq!(log[0].event_id, event_id);
        assert_eq!(log[0].parent_id, Some(parent_id));
        assert_eq!(log[0].correlation_id, correlation_id);
    }
}
