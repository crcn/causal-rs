//! `Reactor` (v0.3) trait — pure decisions producing new facts.
//!
//! Lives at `crate::reactor_v3::Reactor` until Phase 9 renames the file
//! and removes the legacy `crate::reactor::Reactor<D>` builder struct.
//! The two coexist deliberately: the legacy form continues to drive
//! existing engine settle loops while v0.3 work proceeds incrementally.
//!
//! Per C5, reactors are forward-only — never replayable by default.
//! Per C12, output emission goes through a runtime-side outbox with
//! deterministic event_id derivation; details live in Phase 4.
//!
//! Per C11, reactor outputs are appended via the non-OCC `emit` path.
//! Saga-shaped operations needing aggregate-OCC ("emit only if
//! aggregate at version V") MUST be modeled as command handlers
//! (`load<A>` + decide + `append<A>`), not as `Reactor` impls.

use std::any::TypeId;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::contexts::Ctx;
use crate::event_codec::EventCodec;
use crate::fact::Fact;

/// Pure decision producing `Events`. Forward-only; outputs go through
/// the runtime-side outbox.
///
/// # Footgun — self-feedback loops
///
/// A reactor whose output `Events` include a fact that matches its
/// own `Trigger::type_prefix()` will react to its own output. The
/// relay drains the output to the log, the runner picks it up as a
/// new trigger, the reactor fires again, emits again — ad infinitum.
/// The framework does NOT detect this; per-emit prefix-comparison
/// would impose a cost on every reactor.
///
/// Discipline:
/// - Keep input and output type prefixes disjoint, OR
/// - Filter in the reactor body, e.g. inspect `ctx.metadata` for a
///   `_synthetic = true` flag stamped by the producer when emitting
///   reactor output that intentionally re-enters the same prefix.
#[async_trait]
pub trait Reactor: Send + Sync {
    type Trigger: Fact;

    /// Persistent-subscription group name. See [`crate::Projector::GROUP_NAME`]
    /// for semantics — same uniqueness and cursor-key role.
    const GROUP_NAME: &'static str;

    /// Decide on output facts in response to a trigger. Pure — no I/O
    /// to external systems beyond what's exposed via the trigger and
    /// `ctx`. Output type is heterogeneous (`Events`) to accommodate
    /// reactors that emit across multiple Fact enums (e.g. system +
    /// discovery + scheduling).
    async fn react(
        &self,
        trigger: &Self::Trigger,
        ctx: Ctx<'_>,
    ) -> Result<Events>;
}

// ─────────────────────────────────────────────────────────────────────
// Reactor output types
// ─────────────────────────────────────────────────────────────────────
//
// `Events` is the universal `Reactor::react` return type — a
// type-erased collection of output facts that the runtime persists
// through the outbox. The shape currently builds on the legacy
// `crate::event::Event` trait (via `EventOutput::new<E: Event>`);
// P11.d migrates this to `<F: Fact>` so the v0.4 reactor output is
// fully Fact-aligned. Until then, reactors that emit through this
// surface need their output types to impl both `Event` and `Fact`.

/// One unit of reactor output. Eagerly serialized so the runtime can
/// journal it without re-walking the type.
#[derive(Clone)]
pub struct EventOutput {
    pub type_id: TypeId,
    /// Fully-qualified Rust type name (legacy, for Debug).
    pub event_type: String,
    /// Stable durable name from `Event::durable_name()`.
    pub durable_name: String,
    /// Event prefix for codec/aggregator lookup.
    pub event_prefix: String,
    /// Whether this event is persistent (vs in-process-only).
    pub persistent: bool,
    pub payload: serde_json::Value,
    pub(crate) codec: Option<Arc<EventCodec>>,
    /// Original typed event (live dispatch only).
    pub ephemeral: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl EventOutput {
    /// Create from a typed event implementing the legacy `Event` trait.
    pub fn new<E: crate::event::Event>(event: E) -> Self {
        let durable_name = event.durable_name().to_string();
        let event_prefix = E::event_prefix().to_string();
        let persistent = !E::is_ephemeral();
        let payload = serde_json::to_value(&event).expect("Event must be serializable");
        let ephemeral: Arc<dyn std::any::Any + Send + Sync> = Arc::new(event);
        Self {
            type_id: TypeId::of::<E>(),
            event_type: std::any::type_name::<E>().to_string(),
            durable_name,
            event_prefix: event_prefix.clone(),
            persistent,
            payload,
            codec: Some(Arc::new(EventCodec {
                event_prefix,
                type_id: TypeId::of::<E>(),
                decode: Arc::new(|payload| {
                    let event: E = serde_json::from_value(payload.clone())?;
                    Ok(Arc::new(event))
                }),
            })),
            ephemeral: Some(ephemeral),
        }
    }

    /// Reconstruct from a serialized form (replay path; no codec).
    pub fn from_serialized(event_type: String, payload: serde_json::Value) -> Self {
        Self {
            type_id: TypeId::of::<()>(),
            durable_name: event_type.clone(),
            event_prefix: extract_prefix(&event_type).to_string(),
            persistent: true,
            event_type,
            payload,
            codec: None,
            ephemeral: None,
        }
    }
}

/// Extract the category prefix from an event_type / durable_name.
///
/// `"scrape:web_scrape_completed"` → `"scrape"`
/// `"order_placed"` → `"order_placed"` (no colon = whole string)
pub fn extract_prefix(event_type: &str) -> &str {
    event_type.split(':').next().unwrap_or(event_type)
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

    pub fn add<E: crate::event::Event>(mut self, event: E) -> Self {
        self.outputs.push(EventOutput::new(event));
        self
    }

    pub fn push<E: crate::event::Event>(&mut self, event: E) {
        self.outputs.push(EventOutput::new(event));
    }

    pub fn extend(&mut self, other: Events) {
        self.outputs.extend(other.outputs);
    }

    pub fn len(&self) -> usize { self.outputs.len() }
    pub fn is_empty(&self) -> bool { self.outputs.is_empty() }

    pub fn batch<E: crate::event::Event>(items: impl IntoIterator<Item = E>) -> Self {
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

    impl Fact for OrderPlaced {
        const CATEGORY: &'static str = "order";
        fn name(&self) -> &str { "order_placed" }
        fn stream_id(&self) -> Uuid { self.order_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    /// Mock reactor that returns an empty Events collection.
    /// Real reactors would push response facts into the collection.
    struct NoopReactor;

    #[async_trait]
    impl Reactor for NoopReactor {
        type Trigger = OrderPlaced;
        const GROUP_NAME: &'static str = "noop-reactor";
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
        };

        let events = r.react(&trigger, ctx).await.unwrap();
        assert_eq!(events.len(), 0);
    }
}
