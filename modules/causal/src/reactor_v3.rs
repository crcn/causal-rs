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

use anyhow::Result;
use async_trait::async_trait;

use crate::contexts::Ctx;
use crate::fact::Fact;
use crate::reactor::Events;

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
