//! Aggregator registry — manages aggregate definitions and state.
//!
//! When an event is dispatched through the engine, matching aggregators
//! apply it to state managed by the registry's internal DashMap.

use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use uuid::Uuid;

use crate::event_log::EventLogBackend;
use crate::reactor::extract_prefix;
use crate::snapshot_store::SnapshotStore;
use crate::types::{LogCursor, Snapshot, StreamRevision};

// ── Aggregate state snapshots ────────────────────────────────────
//
// The `Aggregate` marker + `Apply<F: Event>` extension traits live in
// `crate::aggregate`. This module focuses on the registry-side
// machinery — type-erased dispatch, state storage, rollback, snapshots.

/// Per-event `(prev, next)` aggregate snapshots produced by a single fold.
///
/// Captured as stack-local pairs around `apply_event`, so transition guards
/// can read the actual prev/next for *this* event — not the racing `:prev`
/// DashMap slot, which is overwritten by every subsequent fold in the same
/// batch. See `tests/fan_in_races.rs::transition_*`.
#[derive(Default)]
pub struct TransitionSnapshots {
    inner: std::collections::HashMap<
        String,
        (Arc<dyn Any + Send + Sync>, Arc<dyn Any + Send + Sync>),
    >,
}

impl TransitionSnapshots {
    /// Empty snapshot map — used for ephemeral events that don't fold.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Look up `(prev, next)` for an aggregate `A` at the given id.
    ///
    /// Returns `None` if this event did not affect `A` for that id, which
    /// transition guards should treat as "no transition — don't fire."
    pub fn get_pair<A: crate::aggregate::Aggregate>(&self, id: Uuid) -> Option<(&A, &A)> {
        let key = format!("{}:{}", A::NAME, id);
        let (pre, post) = self.inner.get(&key)?;
        let pre_a = (**pre).downcast_ref::<A>()?;
        let post_a = (**post).downcast_ref::<A>()?;
        Some((pre_a, post_a))
    }

    pub(crate) fn insert(
        &mut self,
        key: String,
        prev: Arc<dyn Any + Send + Sync>,
        next: Arc<dyn Any + Send + Sync>,
    ) {
        self.inner.insert(key, (prev, next));
    }

    /// Iterate `(key, prev, next)` for every fold captured.
    /// Used by `AggregatorRegistry::notify_observer` to drive the
    /// inspector's `aggregate_folded` hook.
    pub fn iter(
        &self,
    ) -> impl Iterator<
        Item = (&str, &Arc<dyn Any + Send + Sync>, &Arc<dyn Any + Send + Sync>),
    > {
        self.inner
            .iter()
            .map(|(k, (prev, next))| (k.as_str(), prev, next))
    }
}

// ── Aggregator (type-erased event→aggregate applier) ────────────────

/// A type-erased aggregator that maps an event to an aggregate and applies it.
///
/// Clone is cheap — every non-trivial field is `Arc<dyn Fn>`. Used by
/// the `EngineBuilder` to build per-runner registry copies.
#[derive(Clone)]
pub struct Aggregator {
    /// The event prefix for matching (e.g. "scrape", "order_placed").
    pub event_prefix: String,
    /// TypeId of the event for fast matching.
    pub event_type_id: TypeId,
    /// The aggregate type string.
    pub aggregate_type: String,
    /// `Aggregate::STREAM_CATEGORY` — the single stream this aggregate
    /// folds from, for durable restore. `""` = restore disabled.
    pub stream_category: String,
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

/// Coerce a user-supplied id-extraction return into the `Option<Uuid>`
/// the aggregator framework needs. Supports both shapes so user
/// methods registered via `#[aggregator(id_fn = "...")]` can return
/// either `Uuid` (the common case) or `Option<Uuid>` ("skip this
/// aggregator on this fact" semantics).
///
/// Macro-generated code uses this trait; user code rarely calls it
/// directly.
pub trait AggregatorIdValue {
    fn into_aggregator_id(self) -> Option<Uuid>;
}

impl AggregatorIdValue for Uuid {
    fn into_aggregator_id(self) -> Option<Uuid> { Some(self) }
}

impl AggregatorIdValue for Option<Uuid> {
    fn into_aggregator_id(self) -> Option<Uuid> { self }
}

impl Aggregator {
    /// Construct an Aggregator that folds facts of type `F` into
    /// aggregate state `A`. Consumers read the folded state via
    /// `ctx.aggregate::<A>(stream_id)` inside their reactor / projector
    /// body.
    ///
    /// Stream id comes from `Event::stream_id`; aggregate type string
    /// comes from `A::NAME` — explicit, stable across refactorings,
    /// portable to disk (backend `aggregate_type` columns).
    ///
    /// Registration via [`crate::EngineBuilder::with_aggregators`].
    pub fn for_type<A, F>() -> Self
    where
        A: crate::aggregate::Aggregate
            + crate::aggregate::Apply<F>
            + Clone
            + serde::Serialize
            + serde::de::DeserializeOwned,
        F: crate::event::Event,
    {
        Self::for_type_with_id_fn::<A, F, _>(|f: &F| Some(<F as crate::event::Event>::stream_id(f)))
    }

    /// Construct an Aggregator that extracts the aggregate id with a
    /// custom function instead of `Event::stream_id`. Returning `None`
    /// from `id_fn` skips the fold for this aggregator on that event.
    ///
    /// **Use case.** A fact may naturally stream by signal_id (per-
    /// signal facts) but contribute to a per-run aggregate. The fact's
    /// `Event::stream_id` is signal_id (for the per-signal stream that
    /// owns it); the *aggregator* over that fact wants run_id. With
    /// `for_type_with_id_fn`, the same fact type can register two
    /// aggregators with different keys — one keyed by `signal_id`
    /// (default via `for_type`), another keyed by `run_id` (custom
    /// via `for_type_with_id_fn`).
    ///
    pub fn for_type_with_id_fn<A, F, IdFn>(id_fn: IdFn) -> Self
    where
        A: crate::aggregate::Aggregate
            + crate::aggregate::Apply<F>
            + Clone
            + serde::Serialize
            + serde::de::DeserializeOwned,
        F: crate::event::Event,
        IdFn: Fn(&F) -> Option<Uuid> + Send + Sync + 'static,
    {
        let event_prefix = <F as crate::event::Event>::CATEGORY.to_string();
        let event_type_id = TypeId::of::<F>();
        let aggregate_type = <A as crate::aggregate::Aggregate>::NAME.to_string();
        let stream_category = <A as crate::aggregate::Aggregate>::STREAM_CATEGORY.to_string();
        let id_fn = Arc::new(id_fn);
        let id_fn_for_extract = id_fn.clone();

        Self {
            event_prefix,
            event_type_id,
            aggregate_type,
            stream_category,
            json_extract_id: Arc::new(move |payload: &serde_json::Value| -> Option<Uuid> {
                let fact: F = serde_json::from_value(payload.clone()).ok()?;
                id_fn_for_extract(&fact)
            }),
            apply_to: Arc::new(|state: &mut dyn Any, data: serde_json::Value| -> Result<()> {
                let state = state
                    .downcast_mut::<A>()
                    .ok_or_else(|| anyhow::anyhow!("aggregate type mismatch in apply_to"))?;
                let fact: F = serde_json::from_value(data)?;
                <A as crate::aggregate::Apply<F>>::apply(state, &fact);
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
    /// Stream version from the Store (ZERO = never persisted / unknown).
    version: StreamRevision,
    /// Version at which last snapshot was taken (ZERO = never).
    snapshot_at_version: StreamRevision,
}

/// Captured aggregator state for rollback after a failed event-processing attempt.
///
/// Produced by [`AggregatorRegistry::capture_for_rollback`] before
/// `apply_event` mutates state, consumed by
/// [`AggregatorRegistry::restore_state`] when the engine needs to undo the
/// mutation (e.g. projection failure → retry).
pub(crate) struct AggregatorRollback {
    entries: Vec<RollbackEntry>,
}

struct RollbackEntry {
    key: String,
    prev_key: String,
    key_entry: Option<StateEntry>,
    prev_entry: Option<StateEntry>,
}

/// Registry of aggregators with owned in-memory state.
///
/// Holds `Aggregator` definitions plus the current folded state for
/// each `(aggregate_type, aggregate_id)` key. State lives in memory
/// for the registry's lifetime — there is no built-in persistence.
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

    /// Find all aggregators that handle the given durable name.
    ///
    /// Extracts the prefix from the durable name and matches against
    /// registered aggregator prefixes.
    pub fn find_by_durable_name(&self, durable_name: &str) -> Vec<&Aggregator> {
        let prefix = extract_prefix(durable_name);
        self.aggregators
            .iter()
            .filter(|a| a.event_prefix == prefix)
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
    /// 4. Insert post-state + bumped version under the same per-key
    ///    DashMap entry guard that held step 1
    ///
    /// State is stored as concrete types via `Arc<dyn Any>` — zero
    /// serialization overhead.
    ///
    /// # Per-key atomicity
    ///
    /// The RMW above is atomic per key. The DashMap `entry()` guard
    /// holds the per-shard lock for the duration of `apply_event`'s
    /// inner block, so concurrent callers targeting the same key
    /// serialize: caller A reads pre=v0, writes post=v1; caller B
    /// then reads pre=v1, writes post=v2.
    ///
    /// This matters because two paths fold into this registry
    /// concurrently:
    ///   1. `Engine::execute_emit` / `Engine::append` fold
    ///      caller-emitted facts.
    ///   2. `ReactorRunner` folds reactor-emitted facts right after it
    ///      appends them to the log (for `engine.snapshot()` visibility).
    ///
    /// Before the entry-guarded variant landed (see the `Unreleased`
    /// CHANGELOG entry that follows 0.4.6) these could lose updates
    /// on the same stream key under load — a regression test
    /// (`aggregator_apply_event_serializes_concurrent_callers`) pins
    /// the contract.
    ///
    /// **Caveat: the `:prev` slot remains racy under fan-in.** It's
    /// written outside the entry guard (writing it inside risks
    /// deadlocking when `key` and `:prev` hash to the same DashMap
    /// shard). New readers should consume the `(prev, post)` pair
    /// from the returned [`TransitionSnapshots`] instead — those are
    /// captured under the guard and reflect this event's exact
    /// transition.
    pub fn apply_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> TransitionSnapshots {
        let mut snapshots = TransitionSnapshots::empty();
        let prefix = extract_prefix(event_type);
        let matching: Vec<&Aggregator> = self
            .aggregators
            .iter()
            .filter(|a| a.event_prefix == prefix)
            .collect();

        for agg in matching {
            let aggregate_id = match agg.extract_id_from_json(payload) {
                Some(id) => id,
                None => continue,
            };

            let key = format!("{}:{}", agg.aggregate_type, aggregate_id);
            let prev_key = format!("{}:prev", key);

            // Atomic RMW under the DashMap entry guard. Holding the
            // entry serializes concurrent applies on the same key,
            // closing the lost-update race between the caller-emit path
            // (`Engine::execute_emit` / `append`) and the reactor-emit
            // path (`ReactorRunner`).
            let (pre_state, post_state) = {
                use dashmap::mapref::entry::Entry;
                let entry = self.state.entry(key.clone());

                let (pre_state, current_version, snapshot_at) = match &entry {
                    Entry::Occupied(occ) => {
                        let e = occ.get();
                        (e.state.clone(), e.version, e.snapshot_at_version)
                    }
                    Entry::Vacant(_) => {
                        let default: Arc<dyn Any + Send + Sync> =
                            Arc::from(agg.default_state());
                        (default, StreamRevision::ZERO, StreamRevision::ZERO)
                    }
                };

                let mut next_state = agg.clone_state(pre_state.as_ref());
                if let Err(e) = agg.apply_to(next_state.as_mut(), payload.clone()) {
                    tracing::error!("Failed to apply event to aggregate {}: {}", key, e);
                }
                let post_state: Arc<dyn Any + Send + Sync> = Arc::from(next_state);

                let new_entry = StateEntry {
                    state: post_state.clone(),
                    version: StreamRevision::from_raw(current_version.raw() + 1),
                    snapshot_at_version: snapshot_at,
                };
                match entry {
                    Entry::Occupied(mut occ) => {
                        occ.insert(new_entry);
                    }
                    Entry::Vacant(vac) => {
                        vac.insert(new_entry);
                    }
                }

                (pre_state, post_state)
            };

            // `:prev` slot lives on a separate DashMap entry that may
            // hash to a different (or the SAME) shard. Writing it
            // under the entry guard above risks a same-shard
            // deadlock; writing it after release keeps the slot
            // best-effort. The slot is documented racy under fan-in;
            // new code reads the captured transition from
            // `TransitionSnapshots` (returned below).
            self.state.insert(
                prev_key,
                StateEntry {
                    state: pre_state.clone(),
                    version: StreamRevision::ZERO,
                    snapshot_at_version: StreamRevision::ZERO,
                },
            );

            // Capture per-event (pre, post) for transition guards.
            snapshots.insert(key, pre_state, post_state);
        }

        snapshots
    }

    /// Panic unless at least one aggregator was registered for `A`.
    ///
    /// Reading an unregistered aggregate type would silently return
    /// `A::default()` forever — state that never folds is a
    /// configuration bug, not an empty aggregate, and silently
    /// defaulting it is how dedup gates that never fire ship to
    /// production. Fails loudly at the offending call site instead;
    /// the panic is caught by the supervised consumer task.
    fn assert_registered<A>(&self)
    where
        A: crate::aggregate::Aggregate,
    {
        if self.find_first_by_aggregate_type(A::NAME).is_none() {
            panic!(
                "aggregate type `{}` (NAME = \"{}\") was never registered — \
                 no aggregator with this aggregate_type was passed to \
                 EngineBuilder::with_aggregators(...). Its state would \
                 silently stay at Default; register its aggregators.",
                std::any::type_name::<A>(),
                A::NAME,
            );
        }
    }

    /// Get the (prev, next) transition for an aggregate from internal state.
    /// Returns `(A::default(), A::default())` if no state exists yet.
    ///
    /// # Panics
    /// If no aggregator was registered for `A` — see
    /// [`Self::assert_registered`].
    pub fn get_transition<A>(&self, id: Uuid) -> (A, A)
    where
        A: crate::aggregate::Aggregate + Clone,
    {
        self.assert_registered::<A>();
        let key = format!("{}:{}", A::NAME, id);
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
    pub fn get_transition_arc<A>(&self, id: Uuid) -> (Arc<A>, Arc<A>)
    where
        A: crate::aggregate::Aggregate,
    {
        self.assert_registered::<A>();
        let key = format!("{}:{}", A::NAME, id);
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
        A: crate::aggregate::Aggregate + Clone,
    {
        self.get_transition::<A>(Uuid::nil())
    }

    /// Get the (prev, next) transition for a singleton aggregate as `Arc<A>`.
    pub fn get_singleton_arc<A>(&self) -> (Arc<A>, Arc<A>)
    where
        A: crate::aggregate::Aggregate,
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
    pub fn set_state(&self, key: &str, state: Arc<dyn Any + Send + Sync>, version: StreamRevision, snapshot_at_version: StreamRevision) {
        self.state.insert(key.to_string(), StateEntry { state, version, snapshot_at_version });
    }

    /// Read the stream version from the DashMap entry.
    ///
    /// Returns 0 if no state exists (consistent with "version 0 = empty stream").
    pub fn get_version(&self, key: &str) -> StreamRevision {
        self.state
            .get(key)
            .map(|entry| entry.version)
            .unwrap_or(StreamRevision::ZERO)
    }

    /// Read the snapshot_at_version from the DashMap entry.
    ///
    /// Returns 0 if no state exists (consistent with "never snapshotted").
    pub fn get_snapshot_at_version(&self, key: &str) -> StreamRevision {
        self.state
            .get(key)
            .map(|entry| entry.snapshot_at_version)
            .unwrap_or(StreamRevision::ZERO)
    }

    /// Update snapshot_at_version after saving a snapshot.
    pub fn update_snapshot_at_version(&self, key: &str, version: StreamRevision) {
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

    /// The `(aggregate_type, stream_category, id)` triples this event would
    /// fold — one per matching aggregator that yields an id. Used to drive
    /// read-through restore before a runner folds the event.
    pub fn restore_targets(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Vec<(String, String, Uuid)> {
        let prefix = extract_prefix(event_type);
        self.aggregators
            .iter()
            .filter(|a| a.event_prefix == prefix)
            .filter_map(|a| {
                a.extract_id_from_json(payload).map(|id| {
                    (a.aggregate_type.clone(), a.stream_category.clone(), id)
                })
            })
            .collect()
    }

    /// Push each `(key, next)` from a fold's snapshots into the
    /// observer's `aggregate_folded` hook. Used by runners that
    /// folded an event and want to surface state-after-fold to
    /// inspector / telemetry.
    ///
    /// Resolves the matching `Aggregator` by `aggregate_type` (the
    /// key's prefix before `:`) to obtain a typed serializer; the
    /// observer receives JSON, not type-erased `Any`.
    pub fn notify_observer(
        &self,
        snapshots: &TransitionSnapshots,
        observer: &dyn crate::reactor_observer::ReactorObserver,
        correlation_id: Uuid,
        position: LogCursor,
        event_id: Uuid,
    ) {
        for (key, _prev, next) in snapshots.iter() {
            // Key format: "{aggregate_type}:{aggregate_id}". Split
            // once at the first ':'.
            let aggregate_type = key.split(':').next().unwrap_or("");
            let Some(agg) = self.find_first_by_aggregate_type(aggregate_type) else {
                continue;
            };
            match agg.serialize_state(next.as_ref()) {
                Ok(state_json) => observer.aggregate_folded(
                    correlation_id,
                    position,
                    event_id,
                    key,
                    state_json,
                ),
                Err(e) => tracing::warn!(
                    aggregate_key = %key,
                    error = %e,
                    "notify_observer: serialize_state failed; skipping snapshot"
                ),
            }
        }
    }

    /// Capture pre-mutation state for the aggregates that would be affected
    /// by `apply_event(event_type, payload)`. The returned handle can be
    /// passed to [`restore_state`](Self::restore_state) to undo the mutation
    /// — used by the engine to roll back aggregator state when
    /// `process_event_inner` fails after `apply_event` already ran.
    ///
    /// Captures the existing entry for both `key` and `key:prev` (or absence).
    pub(crate) fn capture_for_rollback(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> AggregatorRollback {
        let prefix = extract_prefix(event_type);
        let mut entries = Vec::new();
        for agg in self.aggregators.iter().filter(|a| a.event_prefix == prefix) {
            let aggregate_id = match agg.extract_id_from_json(payload) {
                Some(id) => id,
                None => continue,
            };
            let key = format!("{}:{}", agg.aggregate_type, aggregate_id);
            let prev_key = format!("{}:prev", key);
            let key_entry = self.state.get(&key).map(|e| e.clone());
            let prev_entry = self.state.get(&prev_key).map(|e| e.clone());
            entries.push(RollbackEntry {
                key,
                prev_key,
                key_entry,
                prev_entry,
            });
        }
        AggregatorRollback { entries }
    }

    /// Restore aggregator state captured by [`capture_for_rollback`](Self::capture_for_rollback).
    ///
    /// Each captured entry is restored to its prior value, or removed if it
    /// did not exist before. Idempotent if called twice with the same handle.
    pub(crate) fn restore_state(&self, rollback: AggregatorRollback) {
        for entry in rollback.entries {
            match entry.key_entry {
                Some(state_entry) => {
                    self.state.insert(entry.key, state_entry);
                }
                None => {
                    self.state.remove(&entry.key);
                }
            }
            match entry.prev_entry {
                Some(state_entry) => {
                    self.state.insert(entry.prev_key, state_entry);
                }
                None => {
                    self.state.remove(&entry.prev_key);
                }
            }
        }
    }

    /// Replay events onto an existing state (for snapshot + partial replay).
    ///
    /// Matches events by short type name.
    pub fn replay_events_onto(
        &self,
        aggregate_type: &str,
        state: &mut dyn Any,
        events: &[(&str, &serde_json::Value)],
    ) -> Result<()> {
        for (event_type, payload) in events {
            let prefix = extract_prefix(event_type);
            let matching: Vec<&Aggregator> = self
                .aggregators
                .iter()
                .filter(|a| {
                    a.aggregate_type == aggregate_type
                        && a.event_prefix == prefix
                })
                .collect();

            for agg in matching {
                agg.apply_to(state, (*payload).clone())?;
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

// ── Durable restore / snapshot save (read-through on the aggregate's stream) ──
//
// Free functions so both `Engine` (for `load_aggregate`) and the consumer
// runners (restore-before-fold / save-after-fold) share one implementation.

/// Read-through restore of `(aggregate_type, id)` into `reg` from its own
/// stream `{stream_category}-{id}`: load snapshot (if a store is wired) +
/// replay the stream tail + fold; or replay from genesis if no snapshot.
///
/// Self-heals a snapshot blob that fails to deserialize (delete + rebuild from
/// 0). No-op (returns `false`) when `stream_category` is empty (restore
/// disabled) or the stream is empty and there is no snapshot. Idempotent: if
/// `reg` already has state for the key, returns `true` without I/O.
pub(crate) async fn restore_aggregate(
    reg: &AggregatorRegistry,
    snapshot_store: Option<&dyn SnapshotStore>,
    log: &dyn EventLogBackend,
    aggregate_type: &str,
    stream_category: &str,
    id: Uuid,
) -> Result<bool> {
    if stream_category.is_empty() {
        return Ok(false);
    }
    let key = format!("{aggregate_type}:{id}");
    if reg.has_state(&key) {
        return Ok(true);
    }
    let Some(agg) = reg.find_first_by_aggregate_type(aggregate_type) else {
        return Ok(false);
    };

    let snap = match snapshot_store {
        Some(store) => store.load_snapshot(aggregate_type, id).await?,
        None => None,
    };

    // Seed state from the snapshot (self-heal on a bad blob), tracking the
    // revision the seed represents.
    let (mut state, after, mut last_rev): (
        Box<dyn Any + Send + Sync>,
        Option<StreamRevision>,
        Option<StreamRevision>,
    ) = match &snap {
        Some(s) => match agg.deserialize_state(s.state.clone()) {
            Ok(st) => (st, Some(s.revision), Some(s.revision)),
            Err(e) => {
                tracing::warn!(
                    aggregate = %aggregate_type, %id, error = %e,
                    "snapshot deserialize failed; self-healing (delete + rebuild from 0)"
                );
                if let Some(store) = snapshot_store {
                    let _ = store.delete_snapshot(aggregate_type, id).await;
                }
                (agg.default_state(), None, None)
            }
        },
        None => (agg.default_state(), None, None),
    };
    let had_snapshot = after.is_some();

    // Replay the tail (events with revision > `after`; all of them if `None`).
    let tail = log.read_stream(stream_category, id, after).await?;
    let folded_any = !tail.is_empty();
    if folded_any {
        let pairs: Vec<(&str, &serde_json::Value)> =
            tail.iter().map(|e| (e.event_type.as_str(), &e.payload)).collect();
        reg.replay_events_onto(aggregate_type, state.as_mut(), &pairs)?;
        last_rev = tail.last().map(|e| e.revision);
    }

    // Nothing to restore: no snapshot and an empty stream.
    if !had_snapshot && !folded_any {
        return Ok(false);
    }

    // version = count of events folded = last folded revision + 1.
    let version = last_rev
        .map(|r| StreamRevision::from_raw(r.raw() + 1))
        .unwrap_or(StreamRevision::ZERO);
    let snapshot_at = snap
        .as_ref()
        .map(|s| StreamRevision::from_raw(s.revision.raw() + 1))
        .unwrap_or(StreamRevision::ZERO);
    reg.set_state(&key, Arc::from(state), version, snapshot_at);
    Ok(true)
}

/// Ensure every aggregate this event would fold is restored into `reg` before
/// the live fold — so `ctx.aggregate` reads correct state after a restart.
pub(crate) async fn restore_aggregates_for_event(
    reg: &AggregatorRegistry,
    snapshot_store: Option<&dyn SnapshotStore>,
    log: &dyn EventLogBackend,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    for (aggregate_type, stream_category, id) in reg.restore_targets(event_type, payload) {
        if stream_category.is_empty() {
            continue;
        }
        restore_aggregate(reg, snapshot_store, log, &aggregate_type, &stream_category, id).await?;
    }
    Ok(())
}

/// Save a snapshot for any aggregate in `snapshots` that has folded at least
/// `snapshot_every` events since its last snapshot. Best-effort: a save failure
/// is logged and skipped (the next threshold crossing retries). `revision` is
/// the aggregate stream's last-folded revision (`version - 1`), never `$all`.
pub(crate) async fn maybe_save_snapshots(
    reg: &AggregatorRegistry,
    snapshot_store: &dyn SnapshotStore,
    snapshot_every: u64,
    snapshots: &TransitionSnapshots,
) {
    if snapshot_every == 0 {
        return;
    }
    for (key, _prev, _next) in snapshots.iter() {
        let Some((aggregate_type, id_str)) = key.split_once(':') else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(id_str) else {
            continue;
        };
        let version = reg.get_version(key);
        let snapshot_at = reg.get_snapshot_at_version(key);
        if version.raw().saturating_sub(snapshot_at.raw()) < snapshot_every {
            continue;
        }
        let Some(agg) = reg.find_first_by_aggregate_type(aggregate_type) else {
            continue;
        };
        // Only snapshot restorable aggregates (those with a declared stream).
        if agg.stream_category.is_empty() {
            continue;
        }
        let Some(state) = reg.get_state(key) else {
            continue;
        };
        let state_json = match agg.serialize_state(state.as_ref()) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(aggregate_key = %key, error = %e, "snapshot serialize failed; skipping");
                continue;
            }
        };
        let snapshot = Snapshot {
            aggregate_type: aggregate_type.to_string(),
            aggregate_id: id,
            revision: StreamRevision::from_raw(version.raw().saturating_sub(1)),
            state: state_json,
            created_at: Utc::now(),
        };
        match snapshot_store.save_snapshot(snapshot).await {
            Ok(()) => reg.update_snapshot_at_version(key, version),
            Err(e) => tracing::warn!(aggregate_key = %key, error = %e, "save_snapshot failed; will retry"),
        }
    }
}
