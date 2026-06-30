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
use crate::snapshot_store::SnapshotStore;
use crate::types::{LogCursor, RecordedEvent, Snapshot, StreamRevision};

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
    /// `Aggregate::SUBJECT` — the single stream this aggregate
    /// folds from, for durable restore. `""` = restore disabled.
    pub subject: String,
    /// `Aggregate::INVARIANT` — when true, the engine fences this
    /// aggregate's fact kinds out of `emit` and reactor outputs; the
    /// OCC command path (`Engine::append`) is the only write door.
    pub invariant: bool,
    /// `F::SUBJECT` of the event type this aggregator folds — the
    /// placement category of the stream that holds F's events
    /// (`{event_subject}-{id}`). Drives fold-on-read (`ctx.state_of`
    /// inside partitioned reactors): the subject history to fold is
    /// the event's stream, regardless of whether the *aggregate*
    /// declared a restore SUBJECT.
    pub event_subject: String,
    /// True when this aggregator extracts its key with a custom
    /// `id_fn` (cross-subject fan-in: events streamed by one id,
    /// aggregated under another). Such state cannot be folded from a
    /// single subject history, so fold-on-read rejects it with a
    /// teaching error; serial consumers (projectors) still fold it
    /// from their scan.
    pub custom_id: bool,
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
    /// `ctx.state_of::<A>(subject_id)` inside their reactor / projector
    /// body.
    ///
    /// Stream id comes from `Event::subject_id`; aggregate type string
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
        let mut agg = Self::for_type_with_id_fn::<A, F, _>(|f: &F| {
            Some(<F as crate::event::Event>::subject_id(f))
        });
        // The default key IS the event's subject_id — single-subject
        // fold, eligible for fold-on-read.
        agg.custom_id = false;
        agg
    }

    /// Construct an Aggregator that extracts the aggregate id with a
    /// custom function instead of `Event::subject_id`. Returning `None`
    /// from `id_fn` skips the fold for this aggregator on that event.
    ///
    /// **Use case.** A fact may naturally stream by signal_id (per-
    /// signal facts) but contribute to a per-run aggregate. The fact's
    /// `Event::subject_id` is signal_id (for the per-signal stream that
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
        let event_prefix = <F as crate::event::Event>::NAME.to_string();
        let event_type_id = TypeId::of::<F>();
        let aggregate_type = <A as crate::aggregate::Aggregate>::NAME.to_string();
        let subject = <A as crate::aggregate::Aggregate>::SUBJECT.to_string();
        let invariant = <A as crate::aggregate::Aggregate>::INVARIANT;
        let event_subject = <F as crate::event::Event>::SUBJECT.to_string();
        let id_fn = Arc::new(id_fn);
        let id_fn_for_extract = id_fn.clone();

        Self {
            event_prefix,
            event_type_id,
            aggregate_type,
            subject,
            invariant,
            event_subject,
            // for_type_with_id_fn means the caller supplied a key
            // function; `for_type` (the common path) resets this.
            custom_id: true,
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
    pub subject: String,
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
/// terminal-failure advance never double-count — fold tracks the log, not body
/// success (2026-06-10 audit remediation, Phase A2; the old
/// capture/restore rollback machinery this replaces was deleted —
/// rolling back a fold because a *body* failed desynced state from
/// the cursor permanently).
#[derive(Clone)]
struct StateEntry {
    state: Arc<dyn Any + Send + Sync>,
    /// **Stream-aligned aggregates** (non-empty `subject`):
    /// the next expected revision of the aggregate's own stream —
    /// `last folded revision + 1`; ZERO = nothing folded. Revisions
    /// are dense per stream, so `event.revision != version` detects
    /// both redelivery (`<`, skip) and gaps (`>`, read-through
    /// repair).
    version: StreamRevision,
    /// **Fan-in aggregates** (empty `subject`, e.g. singleton
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
        self.aggregators
            .iter()
            .filter(|a| a.event_prefix == durable_name)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.aggregators.is_empty()
    }

    /// Apply an event to all matching aggregators, using internal state.
    ///
    /// `subject_id` / `category` / `revision` / `position` identify where
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
    /// A *restorable* aggregator (non-empty `subject`) requires
    /// every folded event to live in the aggregate's own stream:
    /// `extract_id(payload) == event.subject_id` and `event.category ==
    /// agg.subject`. Durable restore already hard-requires this
    /// (it replays exactly `{subject}-{id}`); an aggregator
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
        subject_id: Uuid,
        category: &str,
        revision: StreamRevision,
        position: LogCursor,
    ) -> Result<FoldOutcome> {
        let mut outcome = FoldOutcome::default();
        let matching: Vec<&Aggregator> = self
            .aggregators
            .iter()
            .filter(|a| a.event_prefix == event_type)
            .collect();

        for agg in matching {
            let aggregate_id = match agg.extract_id_from_json(payload) {
                Some(id) => id,
                None => continue,
            };
            let aligned = !agg.subject.is_empty();

            // Stream-alignment assertion for restorable aggregates —
            // see the doc comment. Loud, because live fold and durable
            // restore would otherwise silently disagree.
            if aligned && (aggregate_id != subject_id || agg.subject != category) {
                anyhow::bail!(
                    "aggregator for `{}` declares subject `{}` (restorable) but \
                     folded event `{}` from stream `{}-{}` with extracted id {} — a \
                     restorable aggregate must fold exactly its own stream",
                    agg.aggregate_type, agg.subject,
                    event_type, category, subject_id, aggregate_id,
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
                        subject: agg.subject.clone(),
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

    /// Install hydrated state + version into the DashMap — **monotonically**.
    ///
    /// Used by durable restore (a read-through cache fill) and cold-start
    /// hydration. Because `restore_aggregate` reads the stream tail
    /// *asynchronously* before installing, two concurrent restores (or a
    /// restore racing live folds) can each compute state from a different
    /// tail snapshot. An unconditional insert let the one that read
    /// *fewer* events win if it landed last, regressing both the state and
    /// the `version` watermark and silently losing folds (and leaving a
    /// permanently-unhealable gap when the regression tripped a concurrent
    /// repair loop). Folds are deterministic, so a higher `version` is
    /// strictly more state — install only when it advances the entry.
    pub fn set_state(&self, key: &str, state: Arc<dyn Any + Send + Sync>, version: StreamRevision, snapshot_at_version: StreamRevision) {
        use dashmap::mapref::entry::Entry;
        match self.state.entry(key.to_string()) {
            Entry::Occupied(mut occ) => {
                if version.raw() > occ.get().version.raw() {
                    let last_pos = occ.get().last_pos;
                    occ.insert(StateEntry {
                        state,
                        version,
                        // Preserve a live fan-in/position watermark if it's
                        // ahead; restore only knows revisions.
                        last_pos,
                        snapshot_at_version,
                    });
                }
            }
            Entry::Vacant(vac) => {
                vac.insert(StateEntry { state, version, last_pos: LogCursor::ZERO, snapshot_at_version });
            }
        }
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

    /// The `(aggregate_type, subject, id)` triples this event would
    /// fold — one per matching aggregator that yields an id. Used to drive
    /// read-through restore before a runner folds the event.
    pub fn restore_targets(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Vec<(String, String, Uuid)> {
        self.aggregators
            .iter()
            .filter(|a| a.event_prefix == event_type)
            .filter_map(|a| {
                a.extract_id_from_json(payload).map(|id| {
                    (a.aggregate_type.clone(), a.subject.clone(), id)
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
        workflow_id: Uuid,
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
                    workflow_id,
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
            let matching: Vec<&Aggregator> = self
                .aggregators
                .iter()
                .filter(|a| {
                    a.aggregate_type == aggregate_type
                        && a.event_prefix == *event_type
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
// Free functions so both `Engine` (for `state_of`) and the consumer
// runners (restore-before-fold / save-after-fold) share one implementation.

/// Fold one event into `reg`, repairing revision gaps by read-through
/// on the aggregate's own stream. **This is the fold entry point** for
/// every caller with log access (engine emit/append, runners, terminal-failure,
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
    subject_id: Uuid,
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
            reg.apply_event(event_type, payload, subject_id, category, revision, position)?;
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
         (stream {category}-{subject_id}, revision {}) — the aggregate stream \
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
            &gap.subject,
            gap.id,
        )
        .await?
    {
        return Ok(());
    }

    // STRICT-ONLY seed. `advance_watermark` below is a silent no-op on a
    // vacant entry (it only mutates an existing one), so a mixed-root stream
    // — one whose lead revisions belong to events this aggregate does NOT
    // fold — traps repair in a non-converging loop: nothing folds the entry
    // into existence, nothing can advance it. Seed the empty base (default
    // state, version ZERO) so the tail fold/advance carries the watermark
    // forward. ZERO == fold(log[..0]), so the strict
    // `state == fold(log[..cursor])` invariant holds; a genuinely missing
    // revision still fails to converge (read_stream won't return it).
    //
    // The `upto.is_some()` guard keeps this on the strict (consumer) path.
    // The non-strict engine path must NOT seed: it fills vacant entries via
    // the `restore_aggregate` fast-path above, and seeding the shared engine
    // registry would defeat `engine.state_of`, which uses `!has_state` both
    // to trigger restore-on-read and to return `None` for an absent aggregate
    // — a version-ZERO default seed there returns an empty aggregate for one
    // that has events.
    if upto.is_some() && !reg.has_state(&key) {
        if let Some(agg) = reg.find_first_by_aggregate_type(&gap.aggregate_type) {
            reg.set_state(
                &key,
                Arc::from(agg.default_state()),
                StreamRevision::ZERO,
                StreamRevision::ZERO,
            );
        }
    }

    let after = if gap.expected == StreamRevision::ZERO {
        None
    } else {
        Some(StreamRevision::from_raw(gap.expected.raw() - 1))
    };
    let tail = log.read_stream(&gap.subject, gap.id, after).await?;
    for e in &tail {
        if let Some(bound) = upto {
            if e.revision >= bound {
                break;
            }
        }
        let repair_outcome =
            reg.apply_event(&e.event_type, &e.payload, e.subject_id, &e.category, e.revision, e.position)?;
        // Advance THIS aggregate's watermark unless THIS aggregate itself
        // gapped on the event. `apply_event` runs every aggregator matching
        // the event type, so `gaps` can hold a *peer* aggregate's gap when a
        // mixed-root stream carries an event foreign to us but meaningful to
        // another aggregate on the same stream. A peer's gap is not ours: the
        // event is an identity fold for this aggregate, so advancing the
        // watermark is correct (and necessary — otherwise the peer's gap
        // would wedge our repair forever). Only a gap on OUR OWN key signals
        // a concurrent restore/fold mid-flight on this entry, where advancing
        // would jump past an unfolded event and drop a fold (the TOCTOU
        // defect) — so we keep that suppressed and let the outer loop
        // re-detect and re-repair once the racing writer settles.
        let self_gapped = repair_outcome
            .gaps
            .iter()
            .any(|g| g.aggregate_type == gap.aggregate_type && g.id == gap.id);
        if !self_gapped {
            reg.advance_watermark(&key, e.revision, e.position);
        }
    }
    Ok(())
}

/// Read-through restore of `(aggregate_type, id)` into `reg` from its own
/// stream `{subject}-{id}`: load snapshot (if a store is wired) +
/// replay the stream tail + fold; or replay from genesis if no snapshot.
///
/// Self-heals a snapshot blob that fails to deserialize (delete + rebuild from
/// 0). No-op (returns `false`) when `subject` is empty (restore
/// disabled) or the stream is empty and there is no snapshot. Idempotent: if
/// `reg` already has state for the key, returns `true` without I/O.
pub(crate) async fn restore_aggregate(
    reg: &AggregatorRegistry,
    snapshot_store: Option<&dyn SnapshotStore>,
    log: &dyn EventLogBackend,
    aggregate_type: &str,
    subject: &str,
    id: Uuid,
) -> Result<bool> {
    if subject.is_empty() {
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
    let tail = log.read_stream(subject, id, after).await?;
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
        if agg.subject.is_empty() {
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
// Fold-on-read (BLOCKING-1) — position-bounded state for partitioned
// reactors
// ─────────────────────────────────────────────────────────────────────

/// Worker-local incremental cache for [`fold_bounded`]. One per
/// partition worker; dies with its partition (eviction-on-drain), so
/// it never outlives the ordering guarantee that makes it cheap.
///
/// Correctness needs no version check against other writers: the log
/// is append-only, so `fold(events ≤ p)` + `fold(events in (p, b])` ≡
/// `fold(events ≤ b)` for any later bound `b`. The only cache-bypass
/// case is a bound *below* the cached watermark (a cross-subject read
/// at an older position), which folds cold and leaves the cache
/// untouched.
#[derive(Default)]
pub struct FoldOnReadCache {
    entries: parking_lot::Mutex<
        std::collections::HashMap<(String, Uuid), FoldCacheEntry>,
    >,
}

struct FoldCacheEntry {
    /// `fold(stream events with position <= folded_to)`.
    state: Box<dyn Any + Send + Sync>,
    /// Position of the last event folded into `state`.
    folded_to: LogCursor,
    /// Per-stream tail: last revision read from each `{subject}-{id}`
    /// stream (including foreign-kind events that didn't fold) —
    /// the resume point for incremental tail reads.
    stream_tails: std::collections::HashMap<String, StreamRevision>,
}

/// Position-bounded fold of an aggregate's subject history — the state
/// source behind `ctx.state_of` in partitioned reactors.
///
/// Returns `(prev, curr)`: `curr` = fold of all matching events with
/// `position <= bound`; `prev` = the same fold *excluding* the event at
/// exactly `bound` (the trigger), so a reactor reading its own
/// subject sees the transition its trigger caused. When no event of
/// this aggregate sits at `bound`, `prev == curr`.
///
/// The streams to read come from each registered `(A, F)` pair's
/// `event_subject` (`F::SUBJECT`) — the placement of the events that
/// fold into `A` — keyed by `id`. Aggregators registered with a custom
/// `id_fn` (cross-subject fan-in) cannot be folded from one subject
/// history and are rejected with a teaching error.
///
/// # Panics
/// Panics when no aggregator for `aggregate_type` is registered —
/// the same configuration-bug teaching panic `ctx.state_of` always had.
pub(crate) async fn fold_bounded(
    reg: &AggregatorRegistry,
    log: &dyn crate::event_log::EventLogBackend,
    aggregate_type: &str,
    id: Uuid,
    bound: LogCursor,
    cache: &FoldOnReadCache,
) -> Result<(Box<dyn Any + Send + Sync>, Box<dyn Any + Send + Sync>)> {
    let aggs: Vec<&Aggregator> = reg
        .aggregators
        .iter()
        .filter(|a| a.aggregate_type == aggregate_type)
        .collect();
    assert!(
        !aggs.is_empty(),
        "ctx.state_of::<{aggregate_type}>() called but no aggregator for \
         {aggregate_type} was registered with \
         EngineBuilder::with_aggregators(...)",
    );
    if let Some(bad) = aggs.iter().find(|a| a.custom_id) {
        anyhow::bail!(
            "ctx.state_of::<{aggregate_type}>() cannot fold on read: the \
             aggregator over '{}' uses a custom id_fn (cross-subject \
             fan-in), so its state is not a single subject's history. \
             Fold it in a projector-maintained read model, or key the \
             aggregate by the event's own subject_id.",
            bad.event_prefix,
        );
    }

    // The set of subject histories that feed this aggregate.
    let mut streams: Vec<&str> = aggs.iter().map(|a| a.event_subject.as_str()).collect();
    streams.sort_unstable();
    streams.dedup();

    let key = (aggregate_type.to_string(), id);
    // Take the entry out (never hold the lock across log I/O); the
    // worker is this cache's only writer, so take-work-put is race-free.
    let entry = cache.entries.lock().remove(&key);

    let (mut state, mut folded_to, mut tails, cacheable) = match entry {
        Some(e) if e.folded_to <= bound => (e.state, e.folded_to, e.stream_tails, true),
        Some(e) => {
            // Bound below the watermark: fold cold, put the (still
            // valid) entry back untouched.
            cache.entries.lock().insert(key.clone(), e);
            (
                aggs[0].default_state(),
                LogCursor::ZERO,
                std::collections::HashMap::new(),
                false,
            )
        }
        None => (
            aggs[0].default_state(),
            LogCursor::ZERO,
            std::collections::HashMap::new(),
            true,
        ),
    };

    // Read each stream's unfolded tail, bounded at `bound`.
    let mut merged: Vec<RecordedEvent> = Vec::new();
    for s in &streams {
        let after = tails.get(*s).copied();
        let events = log.read_stream(s, id, after).await?;
        merged.extend(
            events
                .into_iter()
                .filter(|e| e.position <= bound && e.position > folded_to),
        );
    }
    merged.sort_by_key(|e| e.position);

    let mut prev: Option<Box<dyn Any + Send + Sync>> = None;
    for event in &merged {
        let Some(agg) = aggs.iter().find(|a| a.event_prefix == event.event_type) else {
            // Foreign kind co-located in the stream — advances the
            // tail (below) but folds nothing.
            tails.insert(event.category.clone(), event.revision);
            folded_to = event.position;
            continue;
        };
        if event.position == bound {
            prev = Some(agg.clone_state(state.as_ref()));
        }
        agg.apply_to(state.as_mut(), event.payload.clone())?;
        tails.insert(event.category.clone(), event.revision);
        folded_to = event.position;
    }

    let curr_clone = aggs[0].clone_state(state.as_ref());
    let prev = prev.unwrap_or_else(|| aggs[0].clone_state(state.as_ref()));
    if cacheable {
        cache.entries.lock().insert(
            key,
            FoldCacheEntry { state, folded_to, stream_tails: tails },
        );
    }
    Ok((prev, curr_clone))
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
        const NAME: &'static str = "pinged";
        fn subject_id(&self) -> Uuid { self.id }
    }

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct PingCount { n: u32 }
    impl Aggregate for PingCount {
        const NAME: &'static str = "PingCount";
        // Restorable / stream-aligned: folds exactly the `ping` stream.
        // This is what selects revision gating + gap repair (an
        // aggregate without SUBJECT is fan-in: position-gated,
        // never repaired, never snapshotted).
        const SUBJECT: &'static str = "ping";
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
                workflow_id: Uuid::new_v4(),
                event_type: "pinged".to_string(),
                payload: serde_json::to_value(&payload).unwrap(),
                created_at: Utc::now(),
                category: Some("ping".into()),
                subject_id: Some(id),
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
            last.subject_id, &last.category, last.revision, last.position,
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
                event.subject_id, &event.category, event.revision, event.position,
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
            &e2.event_type, &e2.payload, e2.subject_id, &e2.category,
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
            "pinged", &payload,
            Uuid::new_v4(), // foreign stream id
            "ping",
            StreamRevision::ZERO,
            LogCursor::from_raw(1),
        ).err().expect("misaligned fold must error");
        assert!(err.to_string().contains("must fold exactly its own stream"),
                "unexpected error: {err:#}");
    }

    // ── Mixed-root stream fixtures ───────────────────────────────────
    // An aligned aggregate `B` over subject `s`, folding only event `b`.
    // Event `a` shares the stream but folds into no aggregator.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct EventB { id: Uuid }
    impl Event for EventB {
        const NAME: &'static str = "b";
        fn subject_id(&self) -> Uuid { self.id }
    }
    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct BCount { n: u32 }
    impl Aggregate for BCount {
        const NAME: &'static str = "B";
        const SUBJECT: &'static str = "s";
    }
    impl Apply<EventB> for BCount {
        fn apply(&mut self, _: &EventB) { self.n += 1; }
    }

    // A second aligned aggregate `C` over the SAME subject `s`, folding only
    // event `c`. Used to exercise mixed-root streams that co-locate two
    // aggregate roots (event `c` is foreign to B but meaningful to C).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct EventC { id: Uuid }
    impl Event for EventC {
        const NAME: &'static str = "c";
        fn subject_id(&self) -> Uuid { self.id }
    }
    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct CCount { n: u32 }
    impl Aggregate for CCount {
        const NAME: &'static str = "C";
        const SUBJECT: &'static str = "s";
    }
    impl Apply<EventC> for CCount {
        fn apply(&mut self, _: &EventC) { self.n += 1; }
    }

    async fn append_raw(store: &MemoryStore, id: Uuid, event_type: &str, payload: serde_json::Value) {
        let ev = EventData {
            event_id: Uuid::new_v4(),
            causation_id: None,
            workflow_id: Uuid::new_v4(),
            event_type: event_type.to_string(),
            payload,
            created_at: Utc::now(),
            category: Some("s".into()),
            subject_id: Some(id),
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        };
        crate::append_event(store, ev).await.unwrap();
    }

    /// Mixed-root stream: revision 0 is an event NO aggregator folds, and
    /// revision 1 is the aggregate's own first folded event. In strict
    /// mode the snapshot-restore fast-path is skipped, so repair relies on
    /// the tail fold/`advance_watermark` — but `advance_watermark` is a
    /// silent no-op on a vacant entry, so without seeding the empty base,
    /// repair re-detects the same gap every round and bails after 8.
    ///
    /// This is the `scout_run` re-extract wedge: a stream whose lead
    /// revision (`enrichment:reextract_completed`) the aggregate doesn't
    /// fold trapped repair in a non-converging loop.
    #[tokio::test]
    async fn strict_repair_seeds_mixed_root_stream() {
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        // rev 0: an event this aggregate does not fold.
        append_raw(&store, id, "a", serde_json::json!({})).await;
        // rev 1: the aggregate's own first event.
        append_raw(&store, id, "b", serde_json::to_value(EventB { id }).unwrap()).await;

        let events = crate::event_log::EventLogBackend::read_all(
            &store, LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events.len(), 2);
        let eb = &events[1];
        assert_eq!(eb.event_type, "b");
        assert_eq!(eb.revision.raw(), 1);

        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<BCount, EventB>());

        // Deliver revision 1 strictly. The aggregate is vacant; revision 1
        // against version 0 is a gap whose lead revision (0) folds into
        // nothing.
        let outcome = fold_event(
            &reg, None, &store,
            &eb.event_type, &eb.payload, eb.subject_id, &eb.category,
            eb.revision, eb.position,
            /* strict_to_event = */ true,
        ).await.expect("mixed-root repair must converge, not bail");

        assert!(outcome.applied, "the delivered event `b` was folded");
        let key = format!("B:{id}");
        assert_eq!(reg.get_version(&key).raw(), 2,
                   "watermark advanced past the delivered revision 1");
        let state = reg.get_state(&key).unwrap();
        assert_eq!(state.downcast_ref::<BCount>().unwrap().n, 1,
                   "exactly event `b` folded — event `a` is not its event");
    }

    /// Multiple contiguous foreign lead revisions: the seed + per-event
    /// advance must carry the watermark across all of them.
    #[tokio::test]
    async fn strict_repair_seeds_all_foreign_lead_run() {
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        append_raw(&store, id, "a", serde_json::json!({})).await;       // rev 0
        append_raw(&store, id, "a", serde_json::json!({})).await;       // rev 1
        append_raw(&store, id, "b", serde_json::to_value(EventB { id }).unwrap()).await; // rev 2

        let events = crate::event_log::EventLogBackend::read_all(
            &store, LogCursor::ZERO, 10,
        ).await.unwrap();
        let eb = &events[2];
        assert_eq!((eb.event_type.as_str(), eb.revision.raw()), ("b", 2));

        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<BCount, EventB>());

        let outcome = fold_event(
            &reg, None, &store,
            &eb.event_type, &eb.payload, eb.subject_id, &eb.category,
            eb.revision, eb.position, true,
        ).await.expect("two foreign lead revisions must still converge");

        assert!(outcome.applied);
        let key = format!("B:{id}");
        assert_eq!(reg.get_version(&key).raw(), 3, "watermark crossed rev0,rev1 to fold rev2");
        assert_eq!(reg.get_state(&key).unwrap().downcast_ref::<BCount>().unwrap().n, 1);
    }

    /// Regression: a foreign event interleaved AFTER a real fold (entry
    /// already exists) — the seed must be a no-op and the pre-existing
    /// advance path must still carry the watermark.
    #[tokio::test]
    async fn strict_repair_foreign_event_after_real_fold() {
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        append_raw(&store, id, "b", serde_json::to_value(EventB { id }).unwrap()).await; // rev 0
        append_raw(&store, id, "a", serde_json::json!({})).await;                        // rev 1
        append_raw(&store, id, "b", serde_json::to_value(EventB { id }).unwrap()).await; // rev 2

        let events = crate::event_log::EventLogBackend::read_all(
            &store, LogCursor::ZERO, 10,
        ).await.unwrap();

        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<BCount, EventB>());

        // Fold rev0 first so the entry exists at version 1.
        let e0 = &events[0];
        fold_event(&reg, None, &store, &e0.event_type, &e0.payload,
                   e0.subject_id, &e0.category, e0.revision, e0.position, true)
            .await.unwrap();
        let key = format!("B:{id}");
        assert_eq!(reg.get_version(&key).raw(), 1);

        // Now deliver rev2; the gap (expected 1) repairs over the foreign rev1.
        let e2 = &events[2];
        fold_event(&reg, None, &store, &e2.event_type, &e2.payload,
                   e2.subject_id, &e2.category, e2.revision, e2.position, true)
            .await.expect("interleaved foreign event must not wedge an existing entry");

        assert_eq!(reg.get_version(&key).raw(), 3);
        assert_eq!(reg.get_state(&key).unwrap().downcast_ref::<BCount>().unwrap().n, 2,
                   "two `b` events folded; the interleaved `a` is not B's");
    }

    /// Two aggregate roots on one stream, peer caught up (6a). During B's
    /// repair the foreign-to-B `c` event is an idempotent skip for C, so it
    /// contributes no gap and B advances normally.
    #[tokio::test]
    async fn strict_repair_shared_stream_peer_caught_up() {
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        append_raw(&store, id, "c", serde_json::to_value(EventC { id }).unwrap()).await; // rev 0
        append_raw(&store, id, "b", serde_json::to_value(EventB { id }).unwrap()).await; // rev 1

        let events = crate::event_log::EventLogBackend::read_all(
            &store, LogCursor::ZERO, 10,
        ).await.unwrap();

        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<BCount, EventB>());
        reg.register(Aggregator::for_type::<CCount, EventC>());

        // In-order processing: fold c@0 (C catches up), then deliver b@1.
        let ec = &events[0];
        fold_event(&reg, None, &store, &ec.event_type, &ec.payload,
                   ec.subject_id, &ec.category, ec.revision, ec.position, true)
            .await.unwrap();
        let eb = &events[1];
        fold_event(&reg, None, &store, &eb.event_type, &eb.payload,
                   eb.subject_id, &eb.category, eb.revision, eb.position, true)
            .await.expect("B converges when peer C is caught up");

        assert_eq!(reg.get_version(&format!("B:{id}")).raw(), 2);
        assert_eq!(reg.get_state(&format!("B:{id}")).unwrap().downcast_ref::<BCount>().unwrap().n, 1);
        assert_eq!(reg.get_state(&format!("C:{id}")).unwrap().downcast_ref::<CCount>().unwrap().n, 1);
    }

    /// Two aggregate roots on one stream, peer BEHIND and gapping mid-tail
    /// (6b). This is the latent hazard change (b) closes: under the old
    /// global `gaps.is_empty()` gate, C's mid-tail gap would suppress B's
    /// advance and B would bail. Under the per-key gate, C's gap is not B's,
    /// so B advances past the foreign `c` events and converges. (The real
    /// runners never deliver in this order — they fold every event in
    /// position order — so this manufactures the state directly.)
    #[tokio::test]
    async fn strict_repair_shared_stream_peer_behind_converges() {
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        append_raw(&store, id, "c", serde_json::to_value(EventC { id }).unwrap()).await; // rev 0
        append_raw(&store, id, "d", serde_json::json!({})).await;                        // rev 1 (foreign to all)
        append_raw(&store, id, "c", serde_json::to_value(EventC { id }).unwrap()).await; // rev 2
        append_raw(&store, id, "b", serde_json::to_value(EventB { id }).unwrap()).await; // rev 3

        let events = crate::event_log::EventLogBackend::read_all(
            &store, LogCursor::ZERO, 10,
        ).await.unwrap();
        let eb = &events[3];
        assert_eq!((eb.event_type.as_str(), eb.revision.raw()), ("b", 3));

        let mut reg = AggregatorRegistry::new();
        reg.register(Aggregator::for_type::<BCount, EventB>());
        reg.register(Aggregator::for_type::<CCount, EventC>());

        // Deliver b@3 to a COLD registry — C is never pre-folded, so during
        // B's repair the tail's c@2 gaps C (C is at version 1 after c@0).
        let outcome = fold_event(
            &reg, None, &store,
            &eb.event_type, &eb.payload, eb.subject_id, &eb.category,
            eb.revision, eb.position, true,
        ).await.expect("a PEER's gap must not wedge B's repair (per-key advance gate)");

        assert!(outcome.applied);
        assert_eq!(reg.get_version(&format!("B:{id}")).raw(), 4,
                   "B advanced across c@0, d@1, c@2 and folded b@3");
        assert_eq!(reg.get_state(&format!("B:{id}")).unwrap().downcast_ref::<BCount>().unwrap().n, 1,
                   "only `b` folded into B");
    }
}
