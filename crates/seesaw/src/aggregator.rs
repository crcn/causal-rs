//! Aggregator registry — purely declarative aggregate definitions.
//!
//! When an event is dispatched through the engine, matching aggregators
//! apply it to state managed by the Runtime. No internal DashMap, no state ownership.

use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use crate::runtime::Runtime;

// ── Aggregate + Apply traits ─────────────────────────────────────

/// Domain aggregate whose state is maintained by the Runtime.
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
    /// Serialize aggregate state to JSON.
    serialize_state: Arc<dyn Fn(&dyn Any) -> Result<serde_json::Value> + Send + Sync>,
    /// Deserialize aggregate state from JSON.
    deserialize_state:
        Arc<dyn Fn(serde_json::Value) -> Result<Box<dyn Any + Send + Sync>> + Send + Sync>,
}

impl Aggregator {
    /// Create a new aggregator for event type `E` folding into aggregate `A`.
    ///
    /// `extract_id` maps the event to the aggregate ID it belongs to.
    pub fn new<E, A, F>(extract_id: F) -> Self
    where
        E: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
        A: Aggregate + Apply<E> + serde::Serialize + serde::de::DeserializeOwned,
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
            default_state: Arc::new(|| -> Box<dyn Any + Send + Sync> { Box::new(A::default()) }),
            serialize_state: Arc::new(|state: &dyn Any| -> Result<serde_json::Value> {
                let s = state
                    .downcast_ref::<A>()
                    .ok_or_else(|| anyhow::anyhow!("aggregate type mismatch in serialize_state"))?;
                Ok(serde_json::to_value(s)?)
            }),
            deserialize_state: Arc::new(
                |value: serde_json::Value| -> Result<Box<dyn Any + Send + Sync>> {
                    let s: A = serde_json::from_value(value)?;
                    Ok(Box::new(s))
                },
            ),
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

    /// Serialize aggregate state to JSON.
    pub fn serialize_state(&self, state: &dyn Any) -> Result<serde_json::Value> {
        (self.serialize_state)(state)
    }

    /// Deserialize aggregate state from JSON.
    pub fn deserialize_state(
        &self,
        value: serde_json::Value,
    ) -> Result<Box<dyn Any + Send + Sync>> {
        (self.deserialize_state)(value)
    }
}

// ── AggregatorRegistry ──────────────────────────────────────────────

/// Purely declarative registry of aggregators. State lives in the Runtime.
pub struct AggregatorRegistry {
    aggregators: Vec<Aggregator>,
}

impl AggregatorRegistry {
    pub fn new() -> Self {
        Self {
            aggregators: Vec::new(),
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

    /// Apply an event to all matching aggregators, using the Runtime for state.
    ///
    /// For each matching aggregator:
    /// 1. Read current state from runtime (or create default)
    /// 2. Clone current state → prev snapshot, store via runtime
    /// 3. Apply event to current state, store via runtime
    pub fn apply_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
        runtime: &dyn Runtime,
    ) {
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
            let prev_key = format!("{}:prev", key);

            // Get or create current state from runtime
            let mut state: Box<dyn Any + Send + Sync> =
                if let Some(json_state) = runtime.get_state(&key) {
                    match agg.deserialize_state(json_state) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(
                                "Failed to deserialize aggregate state {}: {}",
                                key,
                                e
                            );
                            agg.default_state()
                        }
                    }
                } else {
                    agg.default_state()
                };

            // Snapshot prev before applying
            let prev = agg.clone_state(state.as_ref());
            match agg.serialize_state(prev.as_ref()) {
                Ok(prev_json) => runtime.set_state(&prev_key, prev_json),
                Err(e) => {
                    tracing::error!("Failed to serialize prev state {}: {}", prev_key, e)
                }
            }

            // Apply event to current state
            if let Err(e) = agg.apply_to(state.as_mut(), payload.clone()) {
                tracing::error!("Failed to apply event to aggregate {}: {}", key, e);
            }

            // Write updated state back to runtime
            match agg.serialize_state(state.as_ref()) {
                Ok(json_state) => runtime.set_state(&key, json_state),
                Err(e) => {
                    tracing::error!("Failed to serialize aggregate state {}: {}", key, e)
                }
            }
        }
    }

    /// Get the (prev, next) transition for an aggregate from the Runtime.
    ///
    /// Returns `(prev_state, current_state)` by reading from the runtime.
    /// If no state exists, returns `(A::default(), A::default())`.
    pub fn get_transition<A>(&self, id: Uuid, runtime: &dyn Runtime) -> (A, A)
    where
        A: Aggregate + serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        let key = format!("{}:{}", A::aggregate_type(), id);
        let prev_key = format!("{}:prev", key);

        let next = runtime
            .get_state(&key)
            .and_then(|json| serde_json::from_value::<A>(json).ok())
            .unwrap_or_default();

        let prev = runtime
            .get_state(&prev_key)
            .and_then(|json| serde_json::from_value::<A>(json).ok())
            .unwrap_or_default();

        (prev, next)
    }
}

impl Default for AggregatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}
