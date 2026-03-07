//! Aggregator registry — manages aggregate definitions and state.
//!
//! When an event is dispatched through the engine, matching aggregators
//! apply it to state managed by the registry's internal DashMap.

use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;
use uuid::Uuid;

use crate::event_store::event_type_short_name;
use crate::upcaster::UpcasterRegistry;

// ── Aggregate + Apply traits ─────────────────────────────────────

/// Domain aggregate whose state is maintained by the AggregatorRegistry.
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
    /// Serialize aggregate state to JSON (for durable runtimes).
    serialize_state: Arc<dyn Fn(&dyn Any) -> Result<serde_json::Value> + Send + Sync>,
    /// Deserialize aggregate state from JSON (for durable runtimes).
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

    /// Serialize aggregate state to JSON (for durable runtimes).
    pub fn serialize_state(&self, state: &dyn Any) -> Result<serde_json::Value> {
        (self.serialize_state)(state)
    }

    /// Deserialize aggregate state from JSON (for durable runtimes).
    pub fn deserialize_state(
        &self,
        value: serde_json::Value,
    ) -> Result<Box<dyn Any + Send + Sync>> {
        (self.deserialize_state)(value)
    }
}

// ── AggregatorRegistry ──────────────────────────────────────────────

/// State entry that pairs aggregate state with its stream version.
///
/// Version travels with state in a single DashMap entry to avoid
/// split-brain between separate maps.
#[derive(Clone)]
struct StateEntry {
    state: Arc<dyn Any + Send + Sync>,
    /// Stream version from the Store (0 = never persisted / unknown).
    version: u64,
    /// Version at which last snapshot was taken (0 = never).
    snapshot_at_version: u64,
}

/// Registry of aggregators with owned in-memory state.
pub struct AggregatorRegistry {
    aggregators: Vec<Aggregator>,
    state: DashMap<String, StateEntry>,
}

impl AggregatorRegistry {
    pub fn new() -> Self {
        Self {
            aggregators: Vec::new(),
            state: DashMap::new(),
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

    /// Apply an event to all matching aggregators, using internal state.
    ///
    /// For each matching aggregator:
    /// 1. Read current state (or create default)
    /// 2. Clone current state → prev snapshot
    /// 3. Apply event to cloned current state
    /// 4. Increment version atomically with state update
    ///
    /// State is stored as concrete types via `Arc<dyn Any>` — zero serialization overhead.
    pub fn apply_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
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

            // Get current entry, or create default.
            let current_entry = self.state.get(&key).map(|v| v.value().clone());
            let (current_state, current_version, snapshot_at) = match current_entry {
                Some(entry) => (entry.state, entry.version, entry.snapshot_at_version),
                None => {
                    // No state yet — create default, store it, and apply
                    let default = Arc::from(agg.default_state());
                    self.state.insert(key.clone(), StateEntry { state: default, version: 0, snapshot_at_version: 0 });
                    return self.apply_event_inner(agg, &key, &prev_key, payload);
                }
            };

            // Clone current state for mutation
            let mut next_state = agg.clone_state(current_state.as_ref());

            // Store prev snapshot (cheap Arc clone of existing state)
            self.state.insert(prev_key, StateEntry { state: current_state, version: 0, snapshot_at_version: 0 });

            // Apply event to the cloned state
            if let Err(e) = agg.apply_to(next_state.as_mut(), payload.clone()) {
                tracing::error!("Failed to apply event to aggregate {}: {}", key, e);
            }

            // Store updated state with incremented version
            self.state.insert(key, StateEntry {
                state: Arc::from(next_state),
                version: current_version + 1,
                snapshot_at_version: snapshot_at,
            });
        }
    }

    /// Helper for the case where we just created default state and need to re-read.
    fn apply_event_inner(
        &self,
        agg: &Aggregator,
        key: &str,
        prev_key: &str,
        payload: &serde_json::Value,
    ) {
        let current_entry = self.state.get(key).map(|v| v.value().clone()).unwrap();
        let mut next_state = agg.clone_state(current_entry.state.as_ref());

        // Store prev snapshot
        self.state.insert(prev_key.to_string(), StateEntry {
            state: current_entry.state,
            version: 0,
            snapshot_at_version: 0,
        });

        // Apply event
        if let Err(e) = agg.apply_to(next_state.as_mut(), payload.clone()) {
            tracing::error!("Failed to apply event to aggregate {}: {}", key, e);
        }

        // Store updated state
        self.state.insert(key.to_string(), StateEntry {
            state: Arc::from(next_state),
            version: current_entry.version + 1,
            snapshot_at_version: current_entry.snapshot_at_version,
        });
    }

    /// Replay a sequence of persisted events to reconstruct aggregate state.
    ///
    /// Takes `(event_type, payload)` pairs (decoupled from `PersistedEvent`).
    /// Uses short name matching so persisted events (e.g. `"OrderPlaced"`)
    /// match aggregators registered with full type paths.
    ///
    /// Upcasters are applied to each event payload before deserialization,
    /// transforming old schemas to the current version.
    ///
    /// Returns `None` if no aggregators are registered for this aggregate type.
    pub fn replay_events(
        &self,
        aggregate_type: &str,
        events: &[(&str, &serde_json::Value)],
        upcasters: &UpcasterRegistry,
    ) -> Result<Option<Box<dyn Any + Send + Sync>>> {
        // Find any aggregator for this aggregate_type to get default_state
        let first = self
            .aggregators
            .iter()
            .find(|a| a.aggregate_type == aggregate_type);

        let first = match first {
            Some(agg) => agg,
            None => return Ok(None),
        };

        let mut state = first.default_state();

        for (event_type, payload) in events {
            // Apply upcasters before deserialization (schema_version=0 as default)
            let upcasted_payload = upcasters.upcast(event_type, 0, (*payload).clone())?;

            // Find aggregators where the short name matches
            let matching: Vec<&Aggregator> = self
                .aggregators
                .iter()
                .filter(|a| {
                    a.aggregate_type == aggregate_type
                        && event_type_short_name(&a.event_type) == *event_type
                })
                .collect();

            for agg in matching {
                agg.apply_to(state.as_mut(), upcasted_payload.clone())?;
            }
        }

        Ok(Some(state))
    }

    /// Get the (prev, next) transition for an aggregate from internal state.
    ///
    /// Returns `(prev_state, current_state)` by reading from the internal DashMap
    /// and downcasting to the concrete aggregate type. Zero serialization.
    /// If no state exists, returns `(A::default(), A::default())`.
    pub fn get_transition<A>(&self, id: Uuid) -> (A, A)
    where
        A: Aggregate + 'static,
    {
        let key = format!("{}:{}", A::aggregate_type(), id);
        let prev_key = format!("{}:prev", key);

        let next = self
            .state
            .get(&key)
            .and_then(|entry| entry.state.downcast_ref::<A>().cloned())
            .unwrap_or_default();

        let prev = self
            .state
            .get(&prev_key)
            .and_then(|entry| entry.state.downcast_ref::<A>().cloned())
            .unwrap_or_default();

        (prev, next)
    }

    /// Get the (prev, next) transition as `Arc<A>` — zero-clone read access.
    ///
    /// Prefer this over [`get_transition`] when the aggregate is expensive to
    /// clone (e.g. contains `HashMap`s). Returns `Arc::new(A::default())` when
    /// no state exists.
    pub fn get_transition_arc<A>(&self, id: Uuid) -> (Arc<A>, Arc<A>)
    where
        A: Aggregate + 'static,
    {
        let key = format!("{}:{}", A::aggregate_type(), id);
        let prev_key = format!("{}:prev", key);

        let next = self
            .state
            .get(&key)
            .and_then(|entry| entry.state.clone().downcast::<A>().ok())
            .unwrap_or_else(|| Arc::new(A::default()));

        let prev = self
            .state
            .get(&prev_key)
            .and_then(|entry| entry.state.clone().downcast::<A>().ok())
            .unwrap_or_else(|| Arc::new(A::default()));

        (prev, next)
    }

    /// Get the (prev, next) transition for a singleton aggregate (uses `Uuid::nil()`).
    pub fn get_singleton<A>(&self) -> (A, A)
    where
        A: Aggregate + 'static,
    {
        self.get_transition::<A>(Uuid::nil())
    }

    /// Get the (prev, next) transition for a singleton aggregate as `Arc<A>`.
    pub fn get_singleton_arc<A>(&self) -> (Arc<A>, Arc<A>)
    where
        A: Aggregate + 'static,
    {
        self.get_transition_arc::<A>(Uuid::nil())
    }

    // ── Store integration helpers ──────────────────────────────

    /// Check if the DashMap has state for a given aggregate key.
    pub fn has_state(&self, key: &str) -> bool {
        self.state.contains_key(key)
    }

    /// Inject hydrated state + version into the DashMap.
    ///
    /// Used during cold-start hydration from the Store.
    pub fn set_state(&self, key: &str, state: Arc<dyn Any + Send + Sync>, version: u64, snapshot_at_version: u64) {
        self.state.insert(key.to_string(), StateEntry { state, version, snapshot_at_version });
    }

    /// Read the stream version from the DashMap entry.
    ///
    /// Returns 0 if no state exists (consistent with "version 0 = empty stream").
    pub fn get_version(&self, key: &str) -> u64 {
        self.state
            .get(key)
            .map(|entry| entry.version)
            .unwrap_or(0)
    }

    /// Read the snapshot_at_version from the DashMap entry.
    ///
    /// Returns 0 if no state exists (consistent with "never snapshotted").
    pub fn get_snapshot_at_version(&self, key: &str) -> u64 {
        self.state
            .get(key)
            .map(|entry| entry.snapshot_at_version)
            .unwrap_or(0)
    }

    /// Update snapshot_at_version after saving a snapshot.
    pub fn update_snapshot_at_version(&self, key: &str, version: u64) {
        if let Some(mut entry) = self.state.get_mut(key) {
            entry.snapshot_at_version = version;
        }
    }

    /// Get a clone of the state for a given aggregate key.
    ///
    /// Returns `None` if no state exists. Used by `save_snapshot`.
    pub fn get_state(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.state.get(key).map(|entry| entry.state.clone())
    }

    /// Remove cached state for an aggregate, forcing re-hydration on next access.
    ///
    /// Used for multi-node sync: after ingesting foreign events, invalidate
    /// the aggregate so the next settle loop hydrates fresh from the Store.
    pub fn remove_state(&self, key: &str) {
        self.state.remove(key);
        self.state.remove(&format!("{}:prev", key));
    }

    /// Return all unique aggregate type names registered.
    pub fn unique_aggregate_types(&self) -> Vec<&str> {
        let mut seen = std::collections::HashSet::new();
        self.aggregators
            .iter()
            .map(|a| a.aggregate_type.as_str())
            .filter(|t| seen.insert(*t))
            .collect()
    }

    /// Find the first aggregator registered for a given aggregate type string.
    ///
    /// Used to access `deserialize_state` / `default_state` during hydration.
    pub fn find_first_by_aggregate_type(&self, aggregate_type: &str) -> Option<&Aggregator> {
        self.aggregators
            .iter()
            .find(|a| a.aggregate_type == aggregate_type)
    }

    /// Replay events onto an existing state (for snapshot + partial replay).
    ///
    /// Matches events by short type name (same as `replay_events`).
    /// Upcasters are applied before deserialization.
    pub fn replay_events_onto(
        &self,
        aggregate_type: &str,
        state: &mut dyn Any,
        events: &[(&str, &serde_json::Value)],
        upcasters: &UpcasterRegistry,
    ) -> Result<()> {
        for (event_type, payload) in events {
            let upcasted = upcasters.upcast(event_type, 0, (*payload).clone())?;

            let matching: Vec<&Aggregator> = self
                .aggregators
                .iter()
                .filter(|a| {
                    a.aggregate_type == aggregate_type
                        && event_type_short_name(&a.event_type) == *event_type
                })
                .collect();

            for agg in matching {
                agg.apply_to(state, upcasted.clone())?;
            }
        }

        Ok(())
    }
}

impl Default for AggregatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}
