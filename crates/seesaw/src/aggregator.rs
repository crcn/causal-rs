//! Aggregator registry — maintains live in-memory aggregate state.
//!
//! When an event is dispatched through the engine, matching aggregators
//! apply it to live state before handlers run. No persistence, no replay.

use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;
use uuid::Uuid;

// ── Aggregate + Apply traits (moved from es/aggregate.rs) ───────────

/// Domain aggregate whose state is maintained in-memory.
///
/// No event type association — use `Apply<E>` to define per-event state transitions.
pub trait Aggregate: Default + Clone + Send + Sync + 'static {
    /// Unique string identifying this aggregate type.
    fn aggregate_type() -> &'static str;
}

/// Per-event state transition. Implement once per (Aggregate, Event) pair.
///
/// ```ignore
/// impl Apply<OrderPlaced> for Order {
///     fn apply(&mut self, event: OrderPlaced) {
///         self.status = Status::Placed;
///         self.total = event.total;
///     }
/// }
/// ```
pub trait Apply<E> {
    fn apply(&mut self, event: E);
}

// ── Aggregator (type-erased event→aggregate applier) ────────────────

/// A type-erased aggregator that maps an event to an aggregate and applies it.
pub struct Aggregator {
    /// The Rust event type name (for matching dispatched events by string).
    pub event_type: String,
    /// TypeId of the event for fast matching.
    pub event_type_id: TypeId,
    /// The aggregate type string.
    pub aggregate_type: String,
    /// Extract the aggregate ID from JSON payload (deserializes internally).
    json_extract_id: Arc<dyn Fn(&serde_json::Value) -> Option<Uuid> + Send + Sync>,
    /// Deserialize JSON and apply to a type-erased aggregate (&mut dyn Any = &mut A).
    apply_to: Arc<dyn Fn(&mut dyn Any, serde_json::Value) -> Result<()> + Send + Sync>,
    /// Clone a type-erased aggregate state.
    clone_state: Arc<dyn Fn(&dyn Any) -> Box<dyn Any + Send + Sync> + Send + Sync>,
    /// Create a default aggregate state.
    default_state: Arc<dyn Fn() -> Box<dyn Any + Send + Sync> + Send + Sync>,
}

impl Aggregator {
    /// Create a new aggregator for event type `E` folding into aggregate `A`.
    ///
    /// `extract_id` maps the event to the aggregate ID it belongs to.
    pub fn new<E, A, F>(extract_id: F) -> Self
    where
        E: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
        A: Aggregate + Apply<E>,
        F: Fn(&E) -> Uuid + Send + Sync + 'static,
    {
        let event_type = std::any::type_name::<E>().to_string();
        let event_type_id = TypeId::of::<E>();
        let aggregate_type = A::aggregate_type().to_string();

        Self {
            event_type,
            event_type_id,
            aggregate_type,
            json_extract_id: Arc::new(move |payload: &serde_json::Value| -> Option<Uuid> {
                let event: E = serde_json::from_value(payload.clone()).ok()?;
                Some(extract_id(&event))
            }),
            apply_to: Arc::new(|state: &mut dyn Any, data: serde_json::Value| -> Result<()> {
                let state = state
                    .downcast_mut::<A>()
                    .ok_or_else(|| anyhow::anyhow!("aggregate type mismatch in apply_to"))?;
                let event: E = serde_json::from_value(data)?;
                state.apply(event);
                Ok(())
            }),
            clone_state: Arc::new(|state: &dyn Any| -> Box<dyn Any + Send + Sync> {
                let s = state.downcast_ref::<A>().unwrap();
                Box::new(s.clone())
            }),
            default_state: Arc::new(|| -> Box<dyn Any + Send + Sync> {
                Box::new(A::default())
            }),
        }
    }

    /// Extract the aggregate ID from a JSON event payload.
    pub fn extract_id_from_json(&self, payload: &serde_json::Value) -> Option<Uuid> {
        (self.json_extract_id)(payload)
    }

    /// Apply this event's JSON data to a type-erased aggregate state.
    pub fn apply_to(&self, state: &mut dyn Any, data: serde_json::Value) -> Result<()> {
        (self.apply_to)(state, data)
    }

    /// Clone a type-erased aggregate state.
    pub fn clone_state(&self, state: &dyn Any) -> Box<dyn Any + Send + Sync> {
        (self.clone_state)(state)
    }

    /// Create a default aggregate state.
    pub fn default_state(&self) -> Box<dyn Any + Send + Sync> {
        (self.default_state)()
    }
}

// ── AggregatorRegistry ──────────────────────────────────────────────

/// Registry of aggregators with live in-memory state.
pub struct AggregatorRegistry {
    aggregators: Vec<Aggregator>,
    /// Live aggregate state: "{agg_type}:{agg_id}" → current state
    states: DashMap<String, Box<dyn Any + Send + Sync>>,
    /// Previous snapshot before last apply: same key → prev state
    prev_snapshots: DashMap<String, Box<dyn Any + Send + Sync>>,
}

impl AggregatorRegistry {
    pub fn new() -> Self {
        Self {
            aggregators: Vec::new(),
            states: DashMap::new(),
            prev_snapshots: DashMap::new(),
        }
    }

    pub fn register(&mut self, aggregator: Aggregator) {
        self.aggregators.push(aggregator);
    }

    /// Find all aggregators that handle the given event type string.
    pub fn find_by_event_type(&self, event_type: &str) -> Vec<&Aggregator> {
        self.aggregators
            .iter()
            .filter(|a| a.event_type == event_type)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.aggregators.is_empty()
    }

    /// Apply an event to all matching aggregators, maintaining live state.
    ///
    /// For each matching aggregator:
    /// 1. Get-or-create current state via `default_state()`
    /// 2. Clone current state → prev snapshot
    /// 3. Apply event to current state
    pub fn apply_event(&self, event_type: &str, payload: &serde_json::Value) {
        let matching: Vec<&Aggregator> = self
            .aggregators
            .iter()
            .filter(|a| a.event_type == event_type)
            .collect();

        for agg in matching {
            let aggregate_id = match agg.extract_id_from_json(payload) {
                Some(id) => id,
                None => continue,
            };

            let key = format!("{}:{}", agg.aggregate_type, aggregate_id);

            // Get or create current state
            let mut state = self
                .states
                .entry(key.clone())
                .or_insert_with(|| agg.default_state());

            // Snapshot prev before applying
            let prev = agg.clone_state(state.value().as_ref());
            self.prev_snapshots.insert(key.clone(), prev);

            // Apply event to current state
            if let Err(e) = agg.apply_to(state.value_mut().as_mut(), payload.clone()) {
                tracing::error!("Failed to apply event to aggregate {}: {}", key, e);
            }
        }
    }

    /// Get the (prev, next) transition for an aggregate.
    ///
    /// Returns `(prev_state, current_state)` by downcasting from the maps.
    /// If no state exists, returns `(A::default(), A::default())`.
    pub fn get_transition<A: Aggregate + 'static>(&self, id: Uuid) -> (A, A) {
        let key = format!("{}:{}", A::aggregate_type(), id);

        let next = self
            .states
            .get(&key)
            .and_then(|entry| entry.value().downcast_ref::<A>().cloned())
            .unwrap_or_default();

        let prev = self
            .prev_snapshots
            .get(&key)
            .and_then(|entry| entry.value().downcast_ref::<A>().cloned())
            .unwrap_or_default();

        (prev, next)
    }

    /// Clear prev snapshots after event fully processed.
    pub fn clear_snapshots(&self) {
        self.prev_snapshots.clear();
    }
}

impl Default for AggregatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}
