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

/// Result of [`AggregatorRegistry::apply_event`].
#[derive(Default)]
pub struct FoldOutcome {
    /// `(prev, next)` transition pairs per affected aggregate key —
    /// exact on a real fold, reconstructed on an idempotent skip.
    pub snapshots: TransitionSnapshots,
    /// True if at least one aggregator actually mutated state (i.e.
    /// not every match was an idempotent skip). Observers should only
    /// be notified when this is set, so retries don't duplicate
    /// timeline rows.
    pub applied: bool,
    /// Aggregates whose stream has events between their watermark and
    /// this event's revision — fold them via read-through repair
    /// ([`fold_event`]) before this event can fold.
    pub gaps: Vec<FoldGap>,
}

/// One detected fold gap: the aggregate's stream advanced past the
/// in-memory watermark without those events folding (out-of-order
/// arrival between eager fold paths, or a not-yet-restored entry).
pub struct FoldGap {
    pub aggregate_type: String,
    pub stream_category: String,
    pub id: Uuid,
    /// The next revision the entry expects (`ZERO` = nothing folded).
    pub expected: StreamRevision,
}

// ── AggregatorRegistry ──────────────────────────────────────────────

/// State entry that pairs aggregate state with its fold watermarks.
///
/// Watermarks travel with state in a single DashMap entry to avoid
/// split-brain between separate maps. They are what makes folds
/// **idempotent**: `apply_event` is a no-op for an event the entry has
/// already seen, so checkpoint-set retries, crash redelivery, and
/// DLQ-advance never double-count — fold tracks the log, not body
/// success (2026-06-10 audit remediation, Phase A2; the old
/// capture/restore rollback machinery this replaces was deleted —
/// rolling back a fold because a *body* failed desynced state from
/// the cursor permanently).
#[derive(Clone)]
struct StateEntry {
    state: Arc<dyn Any + Send + Sync>,
    /// **Stream-aligned aggregates** (non-empty `stream_category`):
    /// the next expected revision of the aggregate's own stream —
    /// `last folded revision + 1`; ZERO = nothing folded. Revisions
    /// are dense per stream, so `event.revision != version` detects
    /// both redelivery (`<`, skip) and gaps (`>`, read-through
    /// repair).
    version: StreamRevision,
    /// **Fan-in aggregates** (empty `stream_category`, e.g. singleton
    /// id_fn patterns): the `$all` position of the last folded event.
    /// Folds gate on `position > last_pos` — exactly-once under the
    /// in-position-order delivery every consumer runner provides.
    /// (In the shared engine registry, where eager folds from
    /// concurrent appends can arrive out of position order, a racing
    /// older fold is skipped — documented trade against the silent
    /// double-count this replaces. Consumer-registry views are always
    /// exact.)
    last_pos: LogCursor,
    /// Version at which last snapshot was taken (ZERO = never).
    snapshot_at_version: StreamRevision,
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
    /// `stream_id` / `category` / `revision` / `position` identify where
    /// the event sits in the log: its own stream (placement) and its
    /// `$all` position. They drive the **idempotency gate** — see
    /// [`StateEntry`] — and the stream-alignment check below.
    ///
    /// For each matching aggregator:
    /// 1. Read current state (or create default)
    /// 2. Gate: skip if this entry already folded the event; surface a
    ///    [`FoldGap`] if intervening stream events are missing (callers
    ///    use [`fold_event`] which repairs gaps by read-through)
    /// 3. Clone current state → prev snapshot
    /// 4. Apply event to cloned current state — an apply error **fails
    ///    the fold** (and thus the step); live fold and replay agree
    ///    that a bad payload is fatal
    /// 5. Insert post-state + advanced watermarks under the same
    ///    per-key DashMap entry guard that held step 1
    ///
    /// # Stream alignment (asserted)
    ///
    /// A *restorable* aggregator (non-empty `stream_category`) requires
    /// every folded event to live in the aggregate's own stream:
    /// `extract_id(payload) == event.stream_id` and `event.category ==
    /// agg.stream_category`. Durable restore already hard-requires this
    /// (it replays exactly `{stream_category}-{id}`); an aggregator
    /// violating it was *already* broken — now it errors loudly instead
    /// of silently diverging between live fold and restore.
    ///
    /// # Per-key atomicity
    ///
    /// The RMW is atomic per key: the DashMap `entry()` guard holds the
    /// per-shard lock for the duration of the inner block, so concurrent
    /// callers targeting the same key serialize. The idempotency gate
    /// composes with this: caller A folds revision 5; a racing redelivery
    /// of revision 5 skips.
    ///
    /// **Caveat: the `:prev` slot remains racy under fan-in.** It's
    /// written outside the entry guard (writing it inside risks
    /// deadlocking when `key` and `:prev` hash to the same DashMap
    /// shard). New readers should consume the `(prev, post)` pair
    /// from the returned [`TransitionSnapshots`] instead.
    pub fn apply_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
        stream_id: Uuid,
        category: &str,
        revision: StreamRevision,
        position: LogCursor,
    ) -> Result<FoldOutcome> {
        let mut outcome = FoldOutcome::default();
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
            let aligned = !agg.stream_category.is_empty();

            // Stream-alignment assertion for restorable aggregates —
            // see the doc comment. Loud, because live fold and durable
            // restore would otherwise silently disagree.
            if aligned && (aggregate_id != stream_id || agg.stream_category != category) {
                anyhow::bail!(
                    "aggregator for `{}` declares stream_category `{}` (restorable) but \
                     folded event `{}` from stream `{}-{}` with extracted id {} — a \
                     restorable aggregate must fold exactly its own stream",
                    agg.aggregate_type, agg.stream_category,
                    event_type, category, stream_id, aggregate_id,
                );
            }

            let key = format!("{}:{}", agg.aggregate_type, aggregate_id);
            let prev_key = format!("{}:prev", key);

            // Atomic gate + RMW under the DashMap entry guard.
            enum Action {
                Folded {
                    pre: Arc<dyn Any + Send + Sync>,
                    post: Arc<dyn Any + Send + Sync>,
                },
                /// Idempotent skip; `exact_prev` = the skipped event is
                /// the entry's most recent fold, so the `:prev` slot
                /// still holds its true pre-state (retry semantics).
                Skipped { exact_prev: bool },
                Gap { expected: StreamRevision },
            }
            let action = {
                use dashmap::mapref::entry::Entry;
                let entry = self.state.entry(key.clone());

                let existing = match &entry {
                    Entry::Occupied(occ) => Some(occ.get().clone()),
                    Entry::Vacant(_) => None,
                };
                let (pre_state, version, last_pos, snapshot_at) = match &existing {
                    Some(e) => (e.state.clone(), e.version, e.last_pos, e.snapshot_at_version),
                    None => {
                        let default: Arc<dyn Any + Send + Sync> =
                            Arc::from(agg.default_state());
                        (default, StreamRevision::ZERO, LogCursor::ZERO, StreamRevision::ZERO)
                    }
                };

                // ── Idempotency gate ─────────────────────────────
                let gate = if aligned {
                    use std::cmp::Ordering::*;
                    match revision.cmp(&version) {
                        Less => Some(Action::Skipped {
                            exact_prev: revision.raw() + 1 == version.raw(),
                        }),
                        Greater => Some(Action::Gap { expected: version }),
                        Equal => None, // next-in-sequence: fold
                    }
                } else if existing.is_some() {
                    // Fan-in: lexicographic (position, revision). An atomic
                    // multi-fact batch shares one commit position; its
                    // facts are ordered by their per-stream revisions, so
                    // position alone would wrongly skip facts 2..n.
                    let newer = position > last_pos
                        || (position == last_pos && revision.raw() + 1 > version.raw());
                    if newer {
                        None
                    } else {
                        Some(Action::Skipped {
                            exact_prev: position == last_pos
                                && revision.raw() + 1 == version.raw(),
                        })
                    }
                } else {
                    None
                };

                if let Some(skip_or_gap) = gate {
                    skip_or_gap
                } else {
                    let mut next_state = agg.clone_state(pre_state.as_ref());
                    agg.apply_to(next_state.as_mut(), payload.clone())
                        .map_err(|e| anyhow::anyhow!(
                            "fold failed for aggregate {key} on event `{event_type}` \
                             (revision {}): {e:#}", revision.raw(),
                        ))?;
                    let post_state: Arc<dyn Any + Send + Sync> = Arc::from(next_state);

                    let new_entry = StateEntry {
                        state: post_state.clone(),
                        version: StreamRevision::from_raw(revision.raw() + 1),
                        last_pos: position,
                        snapshot_at_version: snapshot_at,
                    };
                    match entry {
                        Entry::Occupied(mut occ) => { occ.insert(new_entry); }
                        Entry::Vacant(vac) => { vac.insert(new_entry); }
                    }
                    Action::Folded { pre: pre_state, post: post_state }
                }
            };

            match action {
                Action::Folded { pre, post } => {
                    // `:prev` slot lives on a separate DashMap entry that
                    // may hash to a different (or the SAME) shard. Writing
                    // it under the entry guard above risks a same-shard
                    // deadlock; writing it after release keeps the slot
                    // best-effort (documented racy under fan-in).
                    self.state.insert(
                        prev_key,
                        StateEntry {
                            state: pre.clone(),
                            version: StreamRevision::ZERO,
                            last_pos: LogCursor::ZERO,
                            snapshot_at_version: StreamRevision::ZERO,
                        },
                    );
                    outcome.applied = true;
                    outcome.snapshots.insert(key, pre, post);
                }
                Action::Skipped { exact_prev } => {
                    // Redelivery of an already-folded event: reproduce the
                    // transition pair so a retried body (e.g. checkpoint-set
                    // failure) sees the same (prev, curr) the first attempt
                    // did. Exact when this was the entry's latest fold;
                    // degenerate (curr, curr) otherwise.
                    let curr = self
                        .state
                        .get(&key)
                        .map(|e| e.state.clone())
                        .unwrap_or_else(|| Arc::from(agg.default_state()));
                    let prev = if exact_prev {
                        self.state
                            .get(&prev_key)
                            .map(|e| e.state.clone())
                            .unwrap_or_else(|| curr.clone())
                    } else {
                        curr.clone()
                    };
                    outcome.snapshots.insert(key, prev, curr);
                }
                Action::Gap { expected } => {
                    outcome.gaps.push(FoldGap {
                        aggregate_type: agg.aggregate_type.clone(),
                        stream_category: agg.stream_category.clone(),
                        id: aggregate_id,
                        expected,
                    });
                }
            }
        }

        Ok(outcome)
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
        self.state.insert(
            key.to_string(),
            StateEntry { state, version, last_pos: LogCursor::ZERO, snapshot_at_version },
        );
    }

    /// Advance an entry's fold watermarks past `revision`/`position`
    /// without mutating state — the identity fold for a stream event
    /// that matches no aggregator. Monotonic; no-op on a vacant entry.
    pub(crate) fn advance_watermark(&self, key: &str, revision: StreamRevision, position: LogCursor) {
        if let Some(mut e) = self.state.get_mut(key) {
            if revision.raw() + 1 > e.version.raw() {
                e.version = StreamRevision::from_raw(revision.raw() + 1);
            }
            if position > e.last_pos {
                e.last_pos = position;
            }
        }
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

/// Fold one event into `reg`, repairing revision gaps by read-through
/// on the aggregate's own stream. **This is the fold entry point** for
/// every caller with log access (engine emit/append, runners, DLQ,
/// hydration); raw [`AggregatorRegistry::apply_event`] never repairs.
///
/// Gaps arise when (a) the entry was never folded/restored (fresh
/// process, lazily-touched aggregate), or (b) eager folds from
/// concurrent appends arrive out of revision order — the later
/// revision's fold reads the missing range (committed by definition:
/// revisions are assigned at commit) before folding. A redelivered
/// already-folded event is an idempotent skip inside `apply_event`.
///
/// `strict_to_event` bounds repair at this event's revision. Consumer
/// runners MUST pass `true`: their registries promise
/// `state == fold(log[..cursor])`, so repair must not fold stream
/// events beyond the one being delivered (and the snapshot-restore
/// fast path, which jumps to the stream tail, is skipped). The shared
/// engine registry passes `false` — it is eager read-your-write state
/// with no cursor, so folding to the tail (and snapshot-accelerated
/// restore) is both safe and desirable.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fold_event(
    reg: &AggregatorRegistry,
    snapshot_store: Option<&dyn SnapshotStore>,
    log: &dyn EventLogBackend,
    event_type: &str,
    payload: &serde_json::Value,
    stream_id: Uuid,
    category: &str,
    revision: StreamRevision,
    position: LogCursor,
    strict_to_event: bool,
) -> Result<FoldOutcome> {
    // Each round strictly advances at least one aggregator's watermark,
    // so rounds are bounded by the number of matching aggregators.
    // 8 is a generous ceiling.
    for _ in 0..8 {
        let outcome =
            reg.apply_event(event_type, payload, stream_id, category, revision, position)?;
        if outcome.gaps.is_empty() {
            return Ok(outcome);
        }
        let bound = strict_to_event.then_some(revision);
        for gap in &outcome.gaps {
            repair_gap(reg, snapshot_store, log, gap, bound).await?;
        }
    }
    anyhow::bail!(
        "fold_event: gap repair did not converge for event `{event_type}` \
         (stream {category}-{stream_id}, revision {}) — the aggregate stream \
         is missing revisions it claims to have",
        revision.raw(),
    )
}

/// Bring one gapped aggregate entry up to date: snapshot-accelerated
/// restore when the entry is vacant (unbounded mode only), otherwise
/// fold the missing stream range through `apply_event` so watermarks
/// advance per event. With `upto = Some(r)`, only events with
/// `revision < r` fold — the caller delivers `r` itself next.
async fn repair_gap(
    reg: &AggregatorRegistry,
    snapshot_store: Option<&dyn SnapshotStore>,
    log: &dyn EventLogBackend,
    gap: &FoldGap,
    upto: Option<StreamRevision>,
) -> Result<()> {
    let key = format!("{}:{}", gap.aggregate_type, gap.id);
    if upto.is_none()
        && !reg.has_state(&key)
        && restore_aggregate(
            reg,
            snapshot_store,
            log,
            &gap.aggregate_type,
            &gap.stream_category,
            gap.id,
        )
        .await?
    {
        return Ok(());
    }

    let after = if gap.expected == StreamRevision::ZERO {
        None
    } else {
        Some(StreamRevision::from_raw(gap.expected.raw() - 1))
    };
    let tail = log.read_stream(&gap.stream_category, gap.id, after).await?;
    for e in &tail {
        if let Some(bound) = upto {
            if e.revision >= bound {
                break;
            }
        }
        // Dense within one stream — no further gaps possible for THIS
        // aggregate; other aggregates' gaps surface to fold_event's loop.
        reg.apply_event(&e.event_type, &e.payload, e.stream_id, &e.category, e.revision, e.position)?;
        // A stream event that matches no aggregator folds as the
        // identity — but the watermark must still advance past it, or
        // the next matching event re-detects the same gap forever
        // (mixed streams: foreign events interleaved with the
        // aggregate's own).
        reg.advance_watermark(&key, e.revision, e.position);
    }
    Ok(())
}

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

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::{Aggregate, Apply};
    use crate::event::Event;
    use crate::memory_store::MemoryStore;
    use crate::types::EventData;
    use chrono::Utc;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Ping { id: Uuid }
    impl Event for Ping {
        const CATEGORY: &'static str = "ping";
        fn event_type(&self) -> &str { "pinged" }
        fn stream_id(&self) -> Uuid { self.id }
    }

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct PingCount { n: u32 }
    impl Aggregate for PingCount {
        const NAME: &'static str = "PingCount";
        // Restorable / stream-aligned: folds exactly the `ping` stream.
        // This is what selects revision gating + gap repair (an
        // aggregate without STREAM_CATEGORY is fan-in: position-gated,
        // never repaired, never snapshotted).
        const STREAM_CATEGORY: &'static str = "ping";
    }
    impl Apply<Ping> for PingCount {
        fn apply(&mut self, _: &Ping) { self.n += 1; }
    }

    async fn append_pings(store: &MemoryStore, id: Uuid, n: usize) {
        for _ in 0..n {
            let payload = Ping { id };
            let ev = EventData {
                event_id: Uuid::new_v4(),
                causation_id: None,
                correlation_id: Uuid::new_v4(),
                event_type: format!("ping:{}", payload.event_type()),
                payload: serde_json::to_value(&payload).unwrap(),
                created_at: Utc::now(),
                category: Some("ping".into()),
                stream_id: Some(id),
                metadata: serde_json::Map::new(),
                ephemeral: None,
                persistent: true,
            };
            crate::append_event(store, ev).await.unwrap();
        }
    }

    /// A2: an out-of-order fold arrival (a later revision folding
    /// before an earlier one — the eager engine-registry race) detects
    /// the gap and heals by read-through on the aggregate's own
    /// stream. The earlier revisions' own folds then skip
    /// idempotently. Pre-A2: out-of-order folds either misordered
    /// state or (with a naive watermark) silently dropped folds.
    #[tokio::test]
    async fn out_of_order_fold_heals_via_gap_repair() {
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        append_pings(&store, id, 3).await;
        let events = crate::event_log::EventLogBackend::read_all(
            &store, LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events.len(), 3);

        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<PingCount, Ping>());

        // Fold the LAST event first — revision 2 against an empty
        // entry → gap → repair reads revisions 0..1 from the stream,
        // folds them, then folds revision 2.
        let last = &events[2];
        let _outcome = fold_event(
            &reg, None, &store,
            &last.event_type, &last.payload,
            last.stream_id, &last.category, last.revision, last.position,
            /* strict_to_event = */ false,
        ).await.unwrap();
        // (In unbounded mode the vacant-entry repair restores through
        // the stream tail — which already includes this event — so the
        // final apply registers as an idempotent skip. State is what
        // matters:)

        let key = format!("PingCount:{id}");
        assert_eq!(reg.get_version(&key).raw(), 3, "watermark at tail");
        let state = reg.get_state(&key).unwrap();
        assert_eq!(state.downcast_ref::<PingCount>().unwrap().n, 3,
                   "repair folded the missing revisions in order");

        // The earlier events' own (late) folds are idempotent skips.
        for event in &events[..2] {
            let outcome = fold_event(
                &reg, None, &store,
                &event.event_type, &event.payload,
                event.stream_id, &event.category, event.revision, event.position,
                false,
            ).await.unwrap();
            assert!(!outcome.applied, "late arrival skips — already folded");
        }
        let state = reg.get_state(&key).unwrap();
        assert_eq!(state.downcast_ref::<PingCount>().unwrap().n, 3,
                   "no double-counting from late arrivals");
    }

    /// Strict mode (consumer registries): repair must NOT fold stream
    /// events at or beyond the event being delivered — the registry
    /// promises `state == fold(log[..cursor])`.
    #[tokio::test]
    async fn strict_gap_repair_stops_at_the_delivered_event() {
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        append_pings(&store, id, 4).await;
        let events = crate::event_log::EventLogBackend::read_all(
            &store, LogCursor::ZERO, 10,
        ).await.unwrap();

        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<PingCount, Ping>());

        // Deliver revision 2 strictly (revisions 3 exists in the log
        // but has not been delivered to this consumer yet).
        let e2 = &events[2];
        fold_event(
            &reg, None, &store,
            &e2.event_type, &e2.payload, e2.stream_id, &e2.category,
            e2.revision, e2.position,
            /* strict_to_event = */ true,
        ).await.unwrap();

        let key = format!("PingCount:{id}");
        assert_eq!(reg.get_version(&key).raw(), 3,
                   "watermark stops at the delivered event — revision 3 not folded");
        let state = reg.get_state(&key).unwrap();
        assert_eq!(state.downcast_ref::<PingCount>().unwrap().n, 3,
                   "exactly revisions 0..=2 folded; the undelivered tail is untouched");
    }

    /// The stream-alignment assertion: a restorable aggregator whose
    /// extracted id disagrees with the event's stream id errors loudly
    /// instead of silently diverging from what restore would rebuild.
    #[tokio::test]
    async fn misaligned_restorable_aggregator_fails_loudly() {
        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<PingCount, Ping>());

        let payload = serde_json::to_value(Ping { id: Uuid::new_v4() }).unwrap();
        // Event claims to live in a DIFFERENT stream than the
        // aggregator extracts from the payload.
        let err = reg.apply_event(
            "ping:pinged", &payload,
            Uuid::new_v4(), // foreign stream id
            "ping",
            StreamRevision::ZERO,
            LogCursor::from_raw(1),
        ).err().expect("misaligned fold must error");
        assert!(err.to_string().contains("must fold exactly its own stream"),
                "unexpected error: {err:#}");
    }
}
