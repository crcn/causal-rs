//! Single `Ctx` type passed to all v0.3 consumer bodies.
//!
//! The audit collapsed three identical structs (`ApplyCtx`,
//! `MaterializeCtx`, `ReactCtx`) into one because the capability
//! boundary that justified separate types — `MaterializeCtx` lacks
//! `view::<V>()`, `ReactCtx` has it, `ApplyCtx` lacks it — went away
//! when the `View` trait was cut. Three identical structs add no
//! safety, just noise; `Ctx` is the result.
//!
//! Critical property — no wall-clock accessor. `ctx.now()` returns
//! the fact's logical `occurred_at`, set at emit by the producer.
//! Replay reproduces byte-identical state because deterministic
//! time is the only reachable time.
//!
//! Phase tagging for migration boundaries lives in `ctx.metadata`
//! (the producer stamps `_phase = "pre_migration"` etc.); the
//! audit cut the dedicated `Phase` enum since no consumer body
//! branched on `is_replay()` / `is_live()` and the design always
//! preferred metadata-based tagging.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::aggregate_v3::Aggregate;
use crate::aggregator::AggregatorRegistry;
use crate::types::LogCursor;

/// Aggregate state snapshot pair returned by [`Ctx::aggregate`] and
/// [`Ctx::aggregate_of`]. `prev` is the state before the current
/// event was folded; `curr` is the state after. Both are `Arc<A>`
/// so reads are zero-clone.
pub struct AggregateState<A> {
    pub prev: Arc<A>,
    pub curr: Arc<A>,
}

/// Per-event metadata carried through the log envelope. Application
/// code may stamp arbitrary keys at emit time (`_run_id`,
/// `_schema_v`, `_phase` for migration boundaries); consumers read
/// them via `ctx.metadata`.
pub type Metadata = serde_json::Map<String, serde_json::Value>;

/// Context passed to every v0.4 consumer body
/// (`Projector::project`, `Reactor::react`, `MultiProjector::project`).
///
/// Deliberately absent: any wall-clock accessor. `ctx.now()` returns
/// `occurred_at`, never the system clock.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub event_id:       Uuid,
    pub log_position:   LogCursor,
    pub occurred_at:    DateTime<Utc>,
    pub correlation_id: Uuid,
    pub metadata:       &'a Metadata,
    /// Optional read-side access to in-process aggregator state folded
    /// from events. `None` if the runner wasn't configured with an
    /// aggregator registry. Use [`Ctx::aggregate`] to query.
    pub(crate) aggregators: Option<&'a Arc<AggregatorRegistry>>,
}

impl<'a> std::fmt::Debug for Ctx<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("event_id", &self.event_id)
            .field("log_position", &self.log_position)
            .field("occurred_at", &self.occurred_at)
            .field("correlation_id", &self.correlation_id)
            .field("metadata", &self.metadata)
            .field("has_aggregators", &self.aggregators.is_some())
            .finish()
    }
}

impl<'a> Ctx<'a> {
    /// The fact's logical occurrence time. Consumers that need a
    /// timestamp use this — never wall-clock — so replay reproduces
    /// state byte-identically.
    #[inline]
    pub fn now(&self) -> DateTime<Utc> { self.occurred_at }

    /// Read singleton aggregate state — `(prev, curr)` snapshots
    /// captured by the runner around folding the current event.
    /// `curr` reflects state INCLUDING the current event because the
    /// runner folds before invoking the consumer body.
    ///
    /// Restores the v0.2.x `ctx.aggregate::<A>()` accessor. Used for
    /// incrementally-built read-only state shared across reactors /
    /// projectors (saga-style PipelineState pattern).
    ///
    /// # Panics
    /// Panics if no aggregators were registered with the engine via
    /// `EngineBuilder::with_aggregators(...)`. Calling `aggregate()`
    /// in a body that has no aggregator wiring is a configuration bug
    /// — the panic surfaces it loudly at the offending call site.
    pub fn aggregate<A>(&self) -> AggregateState<A>
    where
        A: Aggregate,
    {
        let reg = self.aggregators.expect(
            "ctx.aggregate::<A>() called but no aggregators were registered \
             with EngineBuilder::with_aggregators(...)",
        );
        let (prev, curr) = reg.get_singleton_arc::<A>();
        AggregateState { prev, curr }
    }

    /// Read aggregate state for a specific aggregate id (non-singleton
    /// aggregates). Same semantics as [`Self::aggregate`] but takes an
    /// id parameter.
    ///
    /// # Panics
    /// Panics if no aggregators were registered. See [`Self::aggregate`].
    pub fn aggregate_of<A>(&self, id: Uuid) -> AggregateState<A>
    where
        A: Aggregate,
    {
        let reg = self.aggregators.expect(
            "ctx.aggregate_of::<A>(id) called but no aggregators were registered \
             with EngineBuilder::with_aggregators(...)",
        );
        let (prev, curr) = reg.get_transition_arc::<A>(id);
        AggregateState { prev, curr }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_meta() -> Metadata {
        Metadata::new()
    }

    #[test]
    fn ctx_now_returns_occurred_at_not_wall_clock() {
        let occurred = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let meta = fixed_meta();
        let ctx = Ctx {
            event_id:       Uuid::nil(),
            log_position:   LogCursor::ZERO,
            occurred_at:    occurred,
            correlation_id: Uuid::nil(),
            metadata:       &meta,
            aggregators:    None,
        };
        assert_eq!(ctx.now(), occurred);
    }

    #[test]
    fn metadata_is_readable() {
        let mut meta = fixed_meta();
        meta.insert("_phase".into(), serde_json::json!("pre_migration"));
        meta.insert("_schema_v".into(), serde_json::json!(2));

        let ctx = Ctx {
            event_id:       Uuid::nil(),
            log_position:   LogCursor::ZERO,
            occurred_at:    Utc::now(),
            correlation_id: Uuid::nil(),
            metadata:       &meta,
            aggregators:    None,
        };
        assert_eq!(
            ctx.metadata.get("_phase").and_then(|v| v.as_str()),
            Some("pre_migration"),
        );
        assert_eq!(
            ctx.metadata.get("_schema_v").and_then(|v| v.as_i64()),
            Some(2),
        );
    }
}
