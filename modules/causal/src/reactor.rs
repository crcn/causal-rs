//! `Reactor` trait — pure decisions producing new facts.
//!
//! Per C5, reactors are forward-only — never replayable by default.
//! Reactor outputs are appended **directly** to the log by the runner,
//! with deterministic `event_id` derivation so redelivered runs dedup
//! on append (C1) — at-least-once + idempotent, no outbox.
//!
//! Per C11, reactor outputs are appended via the non-OCC `emit` path.
//! Saga-shaped operations needing aggregate-OCC ("emit only if
//! aggregate at version V") MUST be modeled as command handlers
//! (`load<A>` + decide + `append<A>`), not as `Reactor` impls.

use std::any::TypeId;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::contexts::Ctx;
use crate::event::Event;

/// Pure decision producing `Events`. Forward-only; outputs are appended
/// directly to the log by the runner.
///
/// # Footgun — self-feedback loops
///
/// A reactor whose output `Events` include a fact that matches its
/// own `Trigger::type_prefix()` will react to its own output. The
/// runner appends the output to the log, picks it up as a new
/// trigger, the reactor fires again, emits again — ad infinitum.
/// The framework does NOT detect this; per-emit prefix-comparison
/// would impose a cost on every reactor.
///
/// Discipline:
/// - Keep input and output type prefixes disjoint, OR
/// - Filter in the reactor body, e.g. inspect `ctx.metadata` for a
///   `_synthetic = true` flag stamped by the producer when emitting
///   reactor output that intentionally re-enters the same prefix.
///
/// # Concurrent decisions on the same aggregate stream
///
/// The framework does **not** provide write-side concurrency control
/// (no CAS / OCC on the user-facing emit path). When two reactors
/// running in parallel both read `ctx.aggregate::<A>(id)` at version
/// V and each emit a fact for that same stream, both facts append —
/// the second decision was made on stale state. Example:
///
/// ```text
/// t0: state = {placed}
/// t1: ReactorA reads {placed}, decides → emits Cancel
/// t2: ReactorB reads {placed}, decides → emits Ship
/// t3: log = [..., Cancel, Ship]
///     apply(Cancel) → {cancelled}, apply(Ship) → {shipped}
///     // ← inconsistent: shipped after cancelled
/// ```
///
/// Mitigations (all user-side, none enforced by the framework):
///
/// - **Funnel writes through one decision reactor per stream.** If
///   exactly one reactor decides on a given aggregate, no race.
///   This is the most common pattern; most apps end up here naturally.
/// - **Design facts to be commutative or idempotent.** "Ship after
///   Cancel" is a no-op; "Cancel after Ship" rolls back. Order
///   doesn't matter.
/// - **Accept the hazard.** Saga-shaped aggregates (read-only state
///   folded by reactors that don't write back to the same stream)
///   are unaffected — this race only matters when a reactor decides
///   on aggregate state and writes a new event to that aggregate's
///   own stream.
#[async_trait]
pub trait Reactor: Send + Sync {
    type Trigger: Event;

    /// Persistent-subscription group name. See
    /// [`crate::Projector::NAME`] for the full uniqueness
    /// contract (in-builder enforcement, cross-engine caveat).
    const NAME: &'static str;

    /// Decide on output facts in response to a trigger. Pure — no I/O
    /// to external systems beyond what's exposed via the trigger and
    /// `ctx`. Output type is heterogeneous (`Events`) to accommodate
    /// reactors that emit across multiple Event enums (e.g. system +
    /// discovery + scheduling).
    ///
    /// # Cross-category outputs are the norm
    ///
    /// Output facts do **not** need to share the trigger's category.
    /// A reactor that consumes from one category and writes to
    /// another is the common shape — e.g. a lifecycle reactor that
    /// emits scheduling facts:
    ///
    /// ```ignore
    /// impl Reactor for RunCompletion {
    ///     type Trigger = LifecycleEvent;
    ///     const NAME: &'static str = "run_completion";
    ///
    ///     async fn react(
    ///         &self,
    ///         _trigger: &LifecycleEvent,
    ///         _ctx: Ctx<'_>,
    ///     ) -> Result<Events> {
    ///         // Trigger is `lifecycle`; output is `schedule`.
    ///         Ok(Events::new().add(ScheduleEvent::Created { /* ... */ }))
    ///     }
    /// }
    /// ```
    ///
    /// The runtime routes each output to `{CATEGORY}-{stream_id}` per
    /// the output Event's own `CATEGORY` const and `stream_id()` — the
    /// trigger's category never leaks into the destination. Mixing
    /// outputs from different categories in a single `Events` return
    /// is also supported.
    async fn react(
        &self,
        trigger: &Self::Trigger,
        ctx: Ctx<'_>,
    ) -> Result<Events>;

    /// **Optional**. Return a JSON description of what this reactor
    /// would do with the given trigger, BEFORE `react()` is invoked.
    /// Captured by [`crate::ReactorObserver::reactor_description`] for
    /// the inspector's "what's about to happen" pane.
    ///
    /// Default: returns `None` (no description). Reactors that opt in
    /// override this to emit a structured intent — typically a JSON
    /// object like `{"action": "ship", "order_id": "..."}`. Pure,
    /// like `react`; no I/O.
    fn describe(&self, _trigger: &Self::Trigger) -> Option<serde_json::Value> {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────
// Reactor output types
// ─────────────────────────────────────────────────────────────────────
//
// `Events` is the universal `Reactor::react` return type — a
// type-erased collection of output facts that the runtime appends
// directly to the log. `EventOutput::new<F: Event>` derives the
// canonical `{CATEGORY}:{name}` event_type from the Event's trait
// methods, matching what `Engine::emit` writes for caller-emitted
// facts.

/// One unit of reactor output. Eagerly serialized so the runtime can
/// journal it without re-walking the type.
#[derive(Clone)]
pub struct EventOutput {
    pub type_id: TypeId,
    /// Canonical event_type: `format!("{CATEGORY}:{name}")` — same
    /// shape `Engine::emit` writes on the producer side. Field name
    /// kept as `durable_name` for backend-impl compatibility.
    pub durable_name: String,
    /// `Event::CATEGORY` — the ROUTING category (consumer/aggregator
    /// matching, via `durable_name`'s prefix).
    pub event_prefix: String,
    /// `Event::STREAM_CATEGORY` — the physical stream this output is
    /// stored in (`{stream_category}-{stream_id}`). Defaults to
    /// `event_prefix`; differs only when the event overrides it.
    pub stream_category: String,
    /// Stream id from `Event::stream_id()` — which stream within
    /// `stream_category` this output targets.
    pub stream_id: Uuid,
    pub payload: serde_json::Value,
    /// Original typed fact (live dispatch only).
    pub ephemeral: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl EventOutput {
    /// Create from a typed Event. The durable_name is composed as
    /// `format!("{CATEGORY}:{name}")` to match the Kurrent-aligned
    /// event_type shape.
    pub fn new<F: crate::event::Event>(fact: F) -> Self {
        let event_prefix = <F as crate::event::Event>::CATEGORY.to_string();
        let stream_category = <F as crate::event::Event>::STREAM_CATEGORY.to_string();
        let durable_name = crate::event_type::compose(&event_prefix, fact.event_type());
        let stream_id = fact.stream_id();
        let payload = serde_json::to_value(&fact).expect("Event must be serializable");
        let ephemeral: Arc<dyn std::any::Any + Send + Sync> = Arc::new(fact);
        Self {
            type_id: TypeId::of::<F>(),
            durable_name,
            event_prefix,
            stream_category,
            stream_id,
            payload,
            ephemeral: Some(ephemeral),
        }
    }

    /// Reconstruct from a serialized form (replay path; no live
    /// ephemeral copy).
    pub fn from_serialized(
        event_type: String,
        stream_id: Uuid,
        payload: serde_json::Value,
    ) -> Self {
        let event_prefix = extract_prefix(&event_type).to_string();
        Self {
            type_id: TypeId::of::<()>(),
            // Replay/serialized path has no separate stream-category info;
            // default it to the routing prefix (the backward-compat default).
            stream_category: event_prefix.clone(),
            event_prefix,
            durable_name: event_type,
            stream_id,
            payload,
            ephemeral: None,
        }
    }
}

/// Extract the category prefix from an event_type.
///
/// `"scrape:web_scrape_completed"` → `"scrape"`
/// `"order_placed"` → `"order_placed"` (no colon = whole string)
///
/// For the common consumer-side case, prefer
/// [`RecordedEvent::category`](crate::RecordedEvent::category).
pub fn extract_prefix(event_type: &str) -> &str {
    crate::event_type::category_of(event_type)
}

/// Universal return type for [`Reactor::react`]. Builder-style; use
/// `Events::push(fact)` (or the [`events!`](crate::events) macro) to
/// accumulate outputs.
#[derive(Clone, Default)]
pub struct Events {
    pub(crate) outputs: Vec<EventOutput>,
}

impl Events {
    pub fn new() -> Self { Self { outputs: Vec::new() } }

    pub fn add<F: crate::event::Event>(mut self, fact: F) -> Self {
        self.outputs.push(EventOutput::new(fact));
        self
    }

    pub fn push<F: crate::event::Event>(&mut self, fact: F) {
        self.outputs.push(EventOutput::new(fact));
    }

    pub fn extend(&mut self, other: Events) {
        self.outputs.extend(other.outputs);
    }

    pub fn len(&self) -> usize { self.outputs.len() }
    pub fn is_empty(&self) -> bool { self.outputs.is_empty() }

    pub fn batch<F: crate::event::Event>(items: impl IntoIterator<Item = F>) -> Self {
        Self {
            outputs: items.into_iter().map(EventOutput::new).collect(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &EventOutput> {
        self.outputs.iter()
    }

    pub fn into_outputs(self) -> Vec<EventOutput> { self.outputs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contexts::Metadata;
        use crate::types::LogCursor;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct OrderPlaced {
        order_id: Uuid,
        occurred_at: DateTime<Utc>,
    }

    impl Event for OrderPlaced {
        const CATEGORY: &'static str = "order";
        fn event_type(&self) -> &str { "order_placed" }
        fn stream_id(&self) -> Uuid { self.order_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    /// Mock reactor that returns an empty Events collection.
    /// Real reactors would push response facts into the collection.
    struct NoopReactor;

    #[async_trait]
    impl Reactor for NoopReactor {
        type Trigger = OrderPlaced;
        const NAME: &'static str = "noop-reactor";
        async fn react(
            &self,
            _trigger: &OrderPlaced,
            _ctx: Ctx<'_>,
        ) -> Result<Events> {
            Ok(Events::new())
        }
    }

    #[tokio::test]
    async fn reactor_react_compiles_and_returns_events() {
        let r = NoopReactor;
        let meta = Metadata::new();
        let trigger = OrderPlaced {
            order_id: Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let ctx = Ctx {
            event_id:       Uuid::nil(),
            log_position:   LogCursor::ZERO,
            occurred_at:    trigger.occurred_at,
            correlation_id: Uuid::nil(),
            metadata:       &meta,
            aggregators:    None,
            logs:           None,
            effect_store: None,
        };

        let events = r.react(&trigger, ctx).await.unwrap();
        assert_eq!(events.len(), 0);
    }
}
