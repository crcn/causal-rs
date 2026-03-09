//! Event-sourcing helpers and aggregate utilities.
//!
//! The [`Store`](crate::store::Store) trait includes event persistence and
//! snapshot methods directly. This module provides helper functions and
//! the `Versioned<A>` wrapper.

use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

use crate::aggregator::{Aggregate, AggregatorRegistry};
use crate::event_log::EventLog;
use crate::types::{AppendResult, NewEvent, Snapshot};

/// Wrapper that pairs aggregate state with its stream version.
#[derive(Debug, Clone)]
pub struct Versioned<A> {
    pub state: A,
    pub version: u64,
}

/// Extract the short type name from a full Rust type path.
///
/// `"my_crate::events::OrderPlaced"` → `"OrderPlaced"`
pub fn event_type_short_name(full: &str) -> &str {
    full.rsplit("::").next().unwrap_or(full)
}

/// Persist an event to the store.
///
/// Uses the short type name (e.g. `"OrderPlaced"`) for durable storage.
/// Returns the [`AppendResult`] with global position and stream version.
pub async fn persist_event<E, A>(
    store: &dyn EventLog,
    aggregate_id: Uuid,
    event: &E,
) -> Result<AppendResult>
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
            metadata: serde_json::Map::new(),
            ephemeral: None,
        })
        .await
}

/// Save a snapshot of aggregate state from the AggregatorRegistry.
///
/// Serializes the current in-memory state of aggregate `A` at `id` and
/// persists it to the store for future hydration acceleration.
pub async fn save_snapshot<A: Aggregate + serde::Serialize + serde::de::DeserializeOwned>(
    store: &dyn EventLog,
    aggregators: &AggregatorRegistry,
    id: Uuid,
) -> Result<()> {
    let key = format!("{}:{}", A::aggregate_type(), id);
    let version = aggregators.get_version(&key);

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
mod tests {
    use super::{event_type_short_name, NewEvent, Uuid};
    use crate::memory_store::MemoryStore;
    use crate::store::Store;
    use chrono::Utc;

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
            ephemeral: None,
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
            ephemeral: None,
        }
    }

    #[tokio::test]
    async fn empty_stream_returns_empty() {
        let store = MemoryStore::with_persistence();
        let events = store.load_stream("Order", Uuid::new_v4(), None).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn append_and_load_aggregate_events() {
        let store = MemoryStore::with_persistence();
        let id = Uuid::new_v4();

        store.append_event(make_aggregate_event("OrderPlaced", serde_json::json!({"total": 100}), "Order", id)).await.unwrap();
        store.append_event(make_aggregate_event("OrderShipped", serde_json::json!({"tracking": "ABC"}), "Order", id)).await.unwrap();

        let events = store.load_stream("Order", id, None).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].version, Some(1));
        assert_eq!(events[0].event_type, "OrderPlaced");
        assert_eq!(events[1].version, Some(2));
        assert_eq!(events[1].event_type, "OrderShipped");
        assert_eq!(events[0].aggregate_type.as_deref(), Some("Order"));
    }

    #[tokio::test]
    async fn non_aggregate_event_is_persisted() {
        let store = MemoryStore::with_persistence();

        let result = store.append_event(make_new_event("SystemStarted", serde_json::json!({"node": "a"}))).await.unwrap();
        assert!(result.position > 0);

        // Not loadable via load_stream (no aggregate)
        let events = store.load_stream("System", Uuid::new_v4(), None).await.unwrap();
        assert!(events.is_empty());

        // But it's in the global log
        let log = store.global_log().lock();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].event_type, "SystemStarted");
        assert!(log[0].aggregate_type.is_none());
        assert!(log[0].version.is_none());
    }

    #[tokio::test]
    async fn load_stream_from_filters_by_version() {
        let store = MemoryStore::with_persistence();
        let id = Uuid::new_v4();

        let result1 = store.append_event(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id)).await.unwrap();
        store.append_event(make_aggregate_event("OrderShipped", serde_json::json!({}), "Order", id)).await.unwrap();
        store.append_event(make_aggregate_event("OrderDelivered", serde_json::json!({}), "Order", id)).await.unwrap();

        // Filter by stream version (not global position)
        let events = store.load_stream("Order", id, Some(result1.version.unwrap())).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "OrderShipped");
        assert_eq!(events[1].event_type, "OrderDelivered");
    }

    #[tokio::test]
    async fn streams_are_isolated() {
        let store = MemoryStore::with_persistence();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        store.append_event(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id1)).await.unwrap();
        store.append_event(make_aggregate_event("UserCreated", serde_json::json!({}), "User", id2)).await.unwrap();

        let events1 = store.load_stream("Order", id1, None).await.unwrap();
        let events2 = store.load_stream("User", id2, None).await.unwrap();
        assert_eq!(events1.len(), 1);
        assert_eq!(events2.len(), 1);
        assert_eq!(events1[0].event_type, "OrderPlaced");
        assert_eq!(events2[0].event_type, "UserCreated");
    }

    #[tokio::test]
    async fn position_increments_globally() {
        let store = MemoryStore::with_persistence();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        store.append_event(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id1)).await.unwrap();
        store.append_event(make_aggregate_event("UserCreated", serde_json::json!({}), "User", id2)).await.unwrap();

        let events1 = store.load_stream("Order", id1, None).await.unwrap();
        let events2 = store.load_stream("User", id2, None).await.unwrap();

        assert!(events2[0].position > events1[0].position);
    }

    #[tokio::test]
    async fn causal_metadata_round_trips() {
        let store = MemoryStore::with_persistence();
        let event_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();

        store
            .append_event(NewEvent {
                event_id,
                parent_id: Some(parent_id),
                correlation_id,
                event_type: "TestEvent".to_string(),
                payload: serde_json::json!({}),
                created_at: Utc::now(),
                aggregate_type: Some("Test".to_string()),
                aggregate_id: Some(Uuid::new_v4()),
                metadata: serde_json::Map::new(),
                ephemeral: None,
            })
            .await
            .unwrap();

        let log = store.global_log().lock();
        assert_eq!(log[0].event_id, event_id);
        assert_eq!(log[0].parent_id, Some(parent_id));
        assert_eq!(log[0].correlation_id, correlation_id);
    }

    #[tokio::test]
    async fn metadata_round_trips() {
        let store = MemoryStore::with_persistence();
        let mut metadata = serde_json::Map::new();
        metadata.insert("run_id".to_string(), serde_json::json!("scrape-abc123"));
        metadata.insert("schema_v".to_string(), serde_json::json!(1));
        metadata.insert("actor".to_string(), serde_json::json!("bot-7"));

        let id = Uuid::new_v4();
        store
            .append_event(NewEvent {
                event_id: Uuid::new_v4(),
                parent_id: None,
                correlation_id: Uuid::new_v4(),
                event_type: "OrderPlaced".to_string(),
                payload: serde_json::json!({"total": 100}),
                created_at: Utc::now(),
                aggregate_type: Some("Order".to_string()),
                aggregate_id: Some(id),
                metadata: metadata.clone(),
                ephemeral: None,
            })
            .await
            .unwrap();

        let events = store.load_stream("Order", id, None).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].metadata, metadata);
        assert_eq!(events[0].metadata["run_id"], "scrape-abc123");
        assert_eq!(events[0].metadata["schema_v"], 1);
        assert_eq!(events[0].metadata["actor"], "bot-7");
    }

    #[tokio::test]
    async fn empty_metadata_round_trips() {
        let store = MemoryStore::with_persistence();
        let id = Uuid::new_v4();

        store.append_event(make_aggregate_event("OrderPlaced", serde_json::json!({}), "Order", id)).await.unwrap();

        let events = store.load_stream("Order", id, None).await.unwrap();
        assert!(events[0].metadata.is_empty());
    }

    #[tokio::test]
    async fn idempotent_append_returns_existing_position() {
        let store = MemoryStore::with_persistence();
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
            ephemeral: None,
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
            ephemeral: None,
        };

        let result1 = store.append_event(event1).await.unwrap();
        let result2 = store.append_event(event2).await.unwrap();

        assert_eq!(result1, result2);
        let log = store.global_log().lock();
        assert_eq!(log.len(), 1);
    }

    #[tokio::test]
    async fn load_global_from_returns_events_after_position() {
        let store = MemoryStore::with_persistence();

        let mut positions = Vec::new();
        for i in 0..5 {
            let result = store.append_event(make_new_event(&format!("Event{}", i), serde_json::json!({"i": i}))).await.unwrap();
            positions.push(result.position);
        }

        let events = store.load_global_from(positions[1], 2).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].position, positions[2]);
        assert_eq!(events[1].position, positions[3]);
    }

    #[tokio::test]
    async fn load_global_from_respects_limit() {
        let store = MemoryStore::with_persistence();

        for i in 0..10 {
            store.append_event(make_new_event(&format!("Event{}", i), serde_json::json!({}))).await.unwrap();
        }

        let events = store.load_global_from(0, 3).await.unwrap();
        assert_eq!(events.len(), 3);
    }

    #[tokio::test]
    async fn load_global_from_empty_when_caught_up() {
        let store = MemoryStore::with_persistence();

        let result = store.append_event(make_new_event("Event", serde_json::json!({}))).await.unwrap();

        let events = store.load_global_from(result.position, 10).await.unwrap();
        assert!(events.is_empty());
    }
}
