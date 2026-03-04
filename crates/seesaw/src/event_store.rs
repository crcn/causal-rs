//! Event-sourcing helpers, deprecated standalone stores, and aggregate utilities.
//!
//! The [`Store`](crate::store::Store) trait now includes event persistence and
//! snapshot methods directly. This module retains helper functions, the
//! `Versioned<A>` wrapper, and deprecated in-memory store implementations for
//! backward compatibility.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use dashmap::DashMap;

use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

use crate::aggregator::{Aggregate, AggregatorRegistry};
use crate::store::Store;
use crate::types::{NewEvent, PersistedEvent, Snapshot};

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

// ── Deprecated EventStore trait ───────────────────────────────────

/// Persistent event log for all events.
///
/// **Deprecated:** Use [`Store`](crate::store::Store) instead, which now
/// includes `append_event`, `load_stream`, `load_stream_from`, and
/// `load_global_from` methods with default no-op implementations.
#[deprecated(since = "0.18.0", note = "Use the unified Store trait instead")]
pub trait EventStore: Send + Sync {
    fn append(
        &self,
        event: NewEvent,
    ) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + '_>>;

    fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>>;

    fn load_stream_from(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_position: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>>;

    fn load_global_from(
        &self,
        after_position: u64,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>>;
}

// ── Deprecated MemoryEventStore ───────────────────────────────────

/// In-memory event store for testing.
///
/// **Deprecated:** Use a `Store` implementation with `append_event`/`load_stream`
/// overrides instead.
#[deprecated(since = "0.18.0", note = "Use a Store implementation with event persistence methods instead")]
pub struct MemoryEventStore {
    /// The global event log. Public for test assertions.
    pub global_log: Mutex<Vec<PersistedEvent>>,
    global_position: AtomicU64,
}

#[allow(deprecated)]
impl MemoryEventStore {
    pub fn new() -> Self {
        Self {
            global_log: Mutex::new(Vec::new()),
            global_position: AtomicU64::new(1),
        }
    }
}

#[allow(deprecated)]
impl Default for MemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(deprecated)]
impl EventStore for MemoryEventStore {
    fn append(
        &self,
        event: NewEvent,
    ) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + '_>> {
        let mut log = self.global_log.lock().unwrap();

        // Idempotency: if event_id already exists, return existing position
        if let Some(existing) = log.iter().find(|e| e.event_id == event.event_id) {
            return Box::pin(std::future::ready(Ok(existing.position)));
        }

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
            metadata: event.metadata,
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

    fn load_global_from(
        &self,
        after_position: u64,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<PersistedEvent>>> + Send + '_>> {
        let log = self.global_log.lock().unwrap();
        let events: Vec<PersistedEvent> = log
            .iter()
            .filter(|e| e.position > after_position)
            .take(limit)
            .cloned()
            .collect();
        Box::pin(std::future::ready(Ok(events)))
    }
}

// ── Standalone functions ───────────────────────────────────────────

/// Persist an event to the store.
///
/// Uses the short type name (e.g. `"OrderPlaced"`) for durable storage.
/// Returns the global position after appending.
pub async fn persist_event<E, A>(
    store: &dyn Store,
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
        .append_event(NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type,
            payload,
            created_at: Utc::now(),
            aggregate_type: Some(A::aggregate_type().to_string()),
            aggregate_id: Some(aggregate_id),
            metadata: serde_json::Map::new(),
        })
        .await
}

// ── Deprecated SnapshotStore trait ─────────────────────────────────

/// Persistent store for aggregate snapshots.
///
/// **Deprecated:** Use [`Store`](crate::store::Store) instead, which now
/// includes `load_snapshot` and `save_snapshot` methods with default no-op
/// implementations.
#[deprecated(since = "0.18.0", note = "Use the unified Store trait instead")]
pub trait SnapshotStore: Send + Sync {
    fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Snapshot>>> + Send + '_>>;

    fn save_snapshot(
        &self,
        snapshot: Snapshot,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

/// In-memory snapshot store for testing.
///
/// **Deprecated:** Use a `Store` implementation with `load_snapshot`/`save_snapshot`
/// overrides instead.
#[deprecated(since = "0.18.0", note = "Use a Store implementation with snapshot methods instead")]
pub struct MemorySnapshotStore {
    snapshots: DashMap<(String, Uuid), Snapshot>,
}

#[allow(deprecated)]
impl MemorySnapshotStore {
    pub fn new() -> Self {
        Self {
            snapshots: DashMap::new(),
        }
    }
}

#[allow(deprecated)]
impl Default for MemorySnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(deprecated)]
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
/// persists it to the store for future hydration acceleration.
pub async fn save_snapshot<A: Aggregate + serde::Serialize + serde::de::DeserializeOwned>(
    store: &dyn Store,
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

    store
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
#[allow(deprecated)]
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
            metadata: serde_json::Map::new(),
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
            metadata: serde_json::Map::new(),
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

    // persist_event test moved to integration tests (now requires full Store impl)

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
                metadata: serde_json::Map::new(),
            })
            .await
            .unwrap();

        let log = store.global_log.lock().unwrap();
        assert_eq!(log[0].event_id, event_id);
        assert_eq!(log[0].parent_id, Some(parent_id));
        assert_eq!(log[0].correlation_id, correlation_id);
    }

    #[tokio::test]
    async fn metadata_round_trips() {
        let store = MemoryEventStore::new();
        let mut metadata = serde_json::Map::new();
        metadata.insert("run_id".to_string(), serde_json::json!("scrape-abc123"));
        metadata.insert("schema_v".to_string(), serde_json::json!(1));
        metadata.insert("actor".to_string(), serde_json::json!("bot-7"));

        let id = Uuid::new_v4();
        store
            .append(NewEvent {
                event_id: Uuid::new_v4(),
                parent_id: None,
                correlation_id: Uuid::new_v4(),
                event_type: "OrderPlaced".to_string(),
                payload: serde_json::json!({"total": 100}),
                created_at: Utc::now(),
                aggregate_type: Some("Order".to_string()),
                aggregate_id: Some(id),
                metadata: metadata.clone(),
            })
            .await
            .unwrap();

        let events = store.load_stream("Order", id).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].metadata, metadata);
        assert_eq!(events[0].metadata["run_id"], "scrape-abc123");
        assert_eq!(events[0].metadata["schema_v"], 1);
        assert_eq!(events[0].metadata["actor"], "bot-7");
    }

    #[tokio::test]
    async fn empty_metadata_round_trips() {
        let store = MemoryEventStore::new();
        let id = Uuid::new_v4();

        store.append(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id)).await.unwrap();

        let events = store.load_stream("Order", id).await.unwrap();
        assert!(events[0].metadata.is_empty());
    }

    #[tokio::test]
    async fn idempotent_append_returns_existing_position() {
        let store = MemoryEventStore::new();
        let event_id = Uuid::new_v4();

        let event1 = NewEvent {
            event_id,
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: "TestEvent".to_string(),
            payload: serde_json::json!({"v": 1}),
            created_at: Utc::now(),
            aggregate_type: None,
            aggregate_id: None,
            metadata: serde_json::Map::new(),
        };
        let event2 = NewEvent {
            event_id, // same event_id
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: "TestEvent".to_string(),
            payload: serde_json::json!({"v": 2}),
            created_at: Utc::now(),
            aggregate_type: None,
            aggregate_id: None,
            metadata: serde_json::Map::new(),
        };

        let pos1 = store.append(event1).await.unwrap();
        let pos2 = store.append(event2).await.unwrap();

        assert_eq!(pos1, pos2);
        let log = store.global_log.lock().unwrap();
        assert_eq!(log.len(), 1);
    }

    #[tokio::test]
    async fn load_global_from_returns_events_after_position() {
        let store = MemoryEventStore::new();

        let mut positions = Vec::new();
        for i in 0..5 {
            let pos = store.append(make_new_event(&format!("Event{}", i), serde_json::json!({"i": i}))).await.unwrap();
            positions.push(pos);
        }

        let events = store.load_global_from(positions[1], 2).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].position, positions[2]);
        assert_eq!(events[1].position, positions[3]);
    }

    #[tokio::test]
    async fn load_global_from_respects_limit() {
        let store = MemoryEventStore::new();

        for i in 0..10 {
            store.append(make_new_event(&format!("Event{}", i), serde_json::json!({}))).await.unwrap();
        }

        let events = store.load_global_from(0, 3).await.unwrap();
        assert_eq!(events.len(), 3);
    }

    #[tokio::test]
    async fn load_global_from_empty_when_caught_up() {
        let store = MemoryEventStore::new();

        let pos = store.append(make_new_event("Event", serde_json::json!({}))).await.unwrap();

        let events = store.load_global_from(pos, 10).await.unwrap();
        assert!(events.is_empty());
    }
}
