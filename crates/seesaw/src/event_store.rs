//! Persistent event store for event-sourced aggregates.
//!
//! Separate from `MemoryStore` (the settle-loop queue) — `EventStore` is the
//! persistent event log for replay and aggregate reconstruction.

use std::collections::HashMap;
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
    /// Global ordering for cross-aggregate projections.
    pub position: u64,
    /// Per-aggregate stream version (1-based).
    pub version: u64,
    /// The aggregate type string.
    pub aggregate_type: String,
    /// The aggregate instance ID.
    pub aggregate_id: Uuid,
    /// Short stable event type name (e.g. "OrderPlaced").
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// Schema version for upcaster chain (0 = original/current).
    pub schema_version: u32,
    /// When the event was persisted.
    pub created_at: DateTime<Utc>,
}

/// A new event to be appended to a stream.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// Short stable event type name (e.g. "OrderPlaced").
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// Schema version for upcaster chain (0 = original/current).
    pub schema_version: u32,
}

/// Concurrency conflict when appending events.
#[derive(Debug, thiserror::Error)]
#[error("concurrency conflict: expected version {expected}, but stream is at version {actual}")]
pub struct ConcurrencyError {
    pub expected: u64,
    pub actual: u64,
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

/// Persistent event log for event-sourced aggregates.
///
/// Separate from the settle-loop queue. Implementations persist events
/// durably and support replay for aggregate reconstruction.
pub trait EventStore: Send + Sync {
    /// Load all events for an aggregate by its globally-unique ID.
    fn load_events(
        &self,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>>;

    /// Append events to an aggregate stream.
    ///
    /// Returns the new stream version after appending.
    /// Fails with `ConcurrencyError` if `expected_version` doesn't match.
    fn append_events(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected_version: u64,
        events: Vec<NewEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + '_>>;

    /// Load events from a specific version (for snapshot + partial replay).
    fn load_events_from(
        &self,
        aggregate_id: Uuid,
        from_version: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>>;
}

// ── MemoryEventStore ───────────────────────────────────────────────

/// In-memory `EventStore` for testing.
pub struct MemoryEventStore {
    streams: Mutex<HashMap<Uuid, Vec<PersistedEvent>>>,
    global_position: AtomicU64,
}

impl MemoryEventStore {
    pub fn new() -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
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
    fn load_events(
        &self,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>> {
        let streams = self.streams.lock().unwrap();
        let events = streams.get(&aggregate_id).cloned().unwrap_or_default();
        Box::pin(std::future::ready(Ok(events)))
    }

    fn append_events(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected_version: u64,
        events: Vec<NewEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + '_>> {
        let mut streams = self.streams.lock().unwrap();
        let stream = streams.entry(aggregate_id).or_default();

        let current_version = stream.last().map(|e| e.version).unwrap_or(0);
        if current_version != expected_version {
            return Box::pin(std::future::ready(Err(ConcurrencyError {
                expected: expected_version,
                actual: current_version,
            }
            .into())));
        }

        let mut version = current_version;
        for new_event in events {
            version += 1;
            let position = self.global_position.fetch_add(1, Ordering::SeqCst);
            stream.push(PersistedEvent {
                position,
                version,
                aggregate_type: aggregate_type.to_string(),
                aggregate_id,
                event_type: new_event.event_type,
                payload: new_event.payload,
                schema_version: new_event.schema_version,
                created_at: Utc::now(),
            });
        }

        Box::pin(std::future::ready(Ok(version)))
    }

    fn load_events_from(
        &self,
        aggregate_id: Uuid,
        from_version: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>> {
        let streams = self.streams.lock().unwrap();
        let events = streams
            .get(&aggregate_id)
            .map(|stream| {
                stream
                    .iter()
                    .filter(|e| e.version >= from_version)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Box::pin(std::future::ready(Ok(events)))
    }
}

// ── Standalone functions ───────────────────────────────────────────

/// Persist an event to the event store.
///
/// Uses the short type name (e.g. `"OrderPlaced"`) for durable storage.
/// Returns the new stream version after appending.
pub async fn persist_event<E, A>(
    store: &dyn EventStore,
    aggregate_id: Uuid,
    expected_version: u64,
    event: &E,
) -> Result<u64>
where
    E: serde::Serialize + 'static,
    A: Aggregate,
{
    let event_type = event_type_short_name(std::any::type_name::<E>()).to_string();
    let payload = serde_json::to_value(event)?;

    store
        .append_events(
            A::aggregate_type(),
            aggregate_id,
            expected_version,
            vec![NewEvent {
                event_type,
                payload,
                schema_version: 0,
            }],
        )
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

    #[tokio::test]
    async fn empty_stream_returns_empty() {
        let store = MemoryEventStore::new();
        let events = store.load_events(Uuid::new_v4()).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn append_and_load_events() {
        let store = MemoryEventStore::new();
        let id = Uuid::new_v4();

        let version = store
            .append_events(
                "Order",
                id,
                0,
                vec![
                    NewEvent {
                        event_type: "OrderPlaced".to_string(),
                        payload: serde_json::json!({"total": 100}),
                        schema_version: 0,
                    },
                    NewEvent {
                        event_type: "OrderShipped".to_string(),
                        payload: serde_json::json!({"tracking": "ABC"}),
                        schema_version: 0,
                    },
                ],
            )
            .await
            .unwrap();

        assert_eq!(version, 2);

        let events = store.load_events(id).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].version, 1);
        assert_eq!(events[0].event_type, "OrderPlaced");
        assert_eq!(events[1].version, 2);
        assert_eq!(events[1].event_type, "OrderShipped");
        assert_eq!(events[0].aggregate_type, "Order");
    }

    #[tokio::test]
    async fn concurrency_conflict() {
        let store = MemoryEventStore::new();
        let id = Uuid::new_v4();

        store
            .append_events(
                "Order",
                id,
                0,
                vec![NewEvent {
                    event_type: "OrderPlaced".to_string(),
                    payload: serde_json::json!({}),
                    schema_version: 0,
                }],
            )
            .await
            .unwrap();

        // Try to append at wrong version
        let result = store
            .append_events(
                "Order",
                id,
                0, // should be 1
                vec![NewEvent {
                    event_type: "OrderShipped".to_string(),
                    payload: serde_json::json!({}),
                    schema_version: 0,
                }],
            )
            .await;

        let err = result.unwrap_err();
        let concurrency_err = err.downcast_ref::<ConcurrencyError>().unwrap();
        assert_eq!(concurrency_err.expected, 0);
        assert_eq!(concurrency_err.actual, 1);
    }

    #[tokio::test]
    async fn load_events_from_version() {
        let store = MemoryEventStore::new();
        let id = Uuid::new_v4();

        store
            .append_events(
                "Order",
                id,
                0,
                vec![
                    NewEvent {
                        event_type: "OrderPlaced".to_string(),
                        payload: serde_json::json!({}),
                        schema_version: 0,
                    },
                    NewEvent {
                        event_type: "OrderShipped".to_string(),
                        payload: serde_json::json!({}),
                        schema_version: 0,
                    },
                    NewEvent {
                        event_type: "OrderDelivered".to_string(),
                        payload: serde_json::json!({}),
                        schema_version: 0,
                    },
                ],
            )
            .await
            .unwrap();

        let events = store.load_events_from(id, 2).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "OrderShipped");
        assert_eq!(events[1].event_type, "OrderDelivered");
    }

    #[tokio::test]
    async fn streams_are_isolated() {
        let store = MemoryEventStore::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        store
            .append_events(
                "Order",
                id1,
                0,
                vec![NewEvent {
                    event_type: "OrderPlaced".to_string(),
                    payload: serde_json::json!({}),
                    schema_version: 0,
                }],
            )
            .await
            .unwrap();

        store
            .append_events(
                "User",
                id2,
                0,
                vec![NewEvent {
                    event_type: "UserCreated".to_string(),
                    payload: serde_json::json!({}),
                    schema_version: 0,
                }],
            )
            .await
            .unwrap();

        let events1 = store.load_events(id1).await.unwrap();
        let events2 = store.load_events(id2).await.unwrap();
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

        store
            .append_events(
                "Order",
                id1,
                0,
                vec![NewEvent {
                    event_type: "OrderPlaced".to_string(),
                    payload: serde_json::json!({}),
                    schema_version: 0,
                }],
            )
            .await
            .unwrap();

        store
            .append_events(
                "User",
                id2,
                0,
                vec![NewEvent {
                    event_type: "UserCreated".to_string(),
                    payload: serde_json::json!({}),
                    schema_version: 0,
                }],
            )
            .await
            .unwrap();

        let events1 = store.load_events(id1).await.unwrap();
        let events2 = store.load_events(id2).await.unwrap();

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

        let version = persist_event::<OrderPlaced, Order>(
            &store,
            order_id,
            0,
            &OrderPlaced { total: 100 },
        )
        .await
        .unwrap();

        assert_eq!(version, 1);

        let events = store.load_events(order_id).await.unwrap();
        assert_eq!(events.len(), 1);
        // Should use short name, not full module path
        assert_eq!(events[0].event_type, "OrderPlaced");
        assert_eq!(events[0].aggregate_type, "Order");
    }
}
