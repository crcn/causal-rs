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

/// Processing-order requirement a reactor declares for its triggers —
/// the partition key the runner uses for this consumer. Decided in the
/// BLOCKING-4 memo (`docs/plans/2026-06-12-memo-partition-key.md`):
/// there is no global key; each reactor declares its own, so dependence
/// on ordering is reviewable in the same diff as the body it governs.
///
/// Causation already orders parent→child under every variant (a trigger
/// doesn't exist until its producer appended it); this declaration only
/// chooses how *sibling* triggers may interleave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    /// One subject's triggers process in log order, across all
    /// workflows. The default: entity lifecycles and dedup gates are
    /// race-free by construction, and a trigger's own subject history
    /// cannot advance under it mid-reaction.
    ///
    /// Subject-less facts (`Uuid::nil()` subjects) share one partition
    /// per consumer — a high-volume subject-less feed should declare
    /// [`Ordering::None`] instead.
    PerSubject,
    /// One workflow's triggers process in log order, across subjects.
    /// The run-pipeline shape: stages reading run-scoped state.
    PerWorkflow,
    /// No ordering beyond causation — every trigger may process
    /// concurrently. For commutative work (e.g. per-file enrichment
    /// fanned out via workflow-root facts).
    None,
}

/// Pure decision producing `Events`. Forward-only; outputs are appended
/// directly to the log by the runner.
///
/// # Footgun — self-feedback loops
///
/// A reactor whose output `Events` include a fact that matches its
/// own `Trigger::NAME` will react to its own output. The
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
/// running in parallel both read `ctx.state_of::<A>(id)` at version
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

    /// How this reactor's triggers partition for concurrent processing
    /// — see [`Ordering`]. Defaults to [`Ordering::PerSubject`].
    const ORDERING: Ordering = Ordering::PerSubject;

    /// Cap on concurrently executing reactions for this consumer —
    /// the knob that protects a bounded external resource (an API's
    /// concurrency limit, a GPU pool) from unbounded partition
    /// fan-out. Default: unbounded (the partition key is the only
    /// concurrency limit).
    ///
    /// The cap bounds *executing attempts*, not partitions: a worker
    /// waiting out a retry backoff holds no slot (the external
    /// resource isn't in use), so a wedged partition under a cap of 1
    /// delays other partitions only while its attempt actually runs.
    /// Like `ORDERING`, concurrency belongs in the same diff as the
    /// body it governs.
    const MAX_IN_FLIGHT: usize = usize::MAX;

    /// Decide on output facts in response to a trigger. Pure — no I/O
    /// to external systems beyond what's exposed via the trigger and
    /// `ctx`. Output type is heterogeneous (`Events`) to accommodate
    /// reactors that emit across multiple Event enums (e.g. system +
    /// discovery + scheduling).
    ///
    /// # Classify your errors
    ///
    /// Retry policy follows the error's declared class (see
    /// [`crate::failure`]): `.map_err(causal::transient)` for infra
    /// blips (waited out under a liveness-time ceiling),
    /// `causal::poison` for deterministic failures (parks immediately),
    /// `causal::domain` for meaningful operation failures (bounded
    /// attempts). Unclassified errors get domain policy and park
    /// labeled `unclassified`.
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
    /// The runtime routes each output to `{CATEGORY}-{subject_id}` per
    /// the output Event's own `CATEGORY` const and `subject_id()` — the
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

    /// Override the engine-wide retry policy for this reactor. `None`
    /// (the default) inherits the engine's configured policy.
    ///
    /// Use the named constructors for the two common shapes:
    ///
    /// ```ignore
    /// fn retry_policy(&self) -> Option<RetryPolicy> {
    ///     Some(RetryPolicy::exponential(10, 500))  // 10 attempts, 500 ms → 60 s
    /// }
    /// ```
    ///
    /// The `#[reactor]` macro generates this method from flat params:
    /// `#[reactor(name = "...", max_attempts = 10, initial_backoff_ms = 500)]`
    fn retry_policy(&self) -> Option<RetryPolicy> {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────
// RetryPolicy
// ─────────────────────────────────────────────────────────────────────

/// Per-reactor retry budget and backoff shape for domain-class and
/// unclassified errors. Transient errors are governed separately by
/// [`TRANSIENT_CEILING`](crate::reactor_runner::TRANSIENT_CEILING)
/// (liveness time, not attempts); poison parks immediately regardless.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Maximum attempts before a domain / unclassified error parks.
    pub max_attempts: u32,
    /// Delay before the first retry, in milliseconds.
    pub initial_backoff_ms: u64,
    /// Multiplier applied to the delay on each successive attempt.
    pub backoff_multiplier: f64,
    /// Ceiling on the computed delay, in milliseconds.
    pub max_backoff_ms: u64,
}

impl RetryPolicy {
    /// Exponential backoff with sane defaults: multiplier = 2.0,
    /// ceiling = 60 seconds.
    pub fn exponential(max_attempts: u32, initial_backoff_ms: u64) -> Self {
        Self {
            max_attempts,
            initial_backoff_ms,
            backoff_multiplier: 2.0,
            max_backoff_ms: 60_000,
        }
    }

    /// Fixed delay — no growth between attempts.
    pub fn fixed(max_attempts: u32, delay_ms: u64) -> Self {
        Self {
            max_attempts,
            initial_backoff_ms: delay_ms,
            backoff_multiplier: 1.0,
            max_backoff_ms: delay_ms,
        }
    }

    /// Engine-default policy shaped from a bare `max_attempts` count,
    /// preserving the historical backoff constants (25 ms base, ×2, 5 s cap).
    pub(crate) fn from_max_attempts(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            initial_backoff_ms: 25,
            backoff_multiplier: 2.0,
            max_backoff_ms: 5_000,
        }
    }

    /// Compute the sleep duration before attempt `n` (0-indexed).
    pub(crate) fn backoff_for(&self, attempt: u32) -> std::time::Duration {
        let base = std::time::Duration::from_millis(self.initial_backoff_ms);
        // cap the exponent to avoid f64 overflow on very high attempt counts
        let exp = self.backoff_multiplier.powi(attempt.min(63) as i32);
        base.mul_f64(exp)
            .min(std::time::Duration::from_millis(self.max_backoff_ms))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Reactor output types
// ─────────────────────────────────────────────────────────────────────
//
// `Events` is the universal `Reactor::react` return type — a
// type-erased collection of output facts that the runtime appends
// directly to the log. `EventOutput::new<F: Event>` carries the fact's
// `NAME` as its event_type — verbatim, matching what `Engine::emit`
// writes for caller-emitted facts.

/// One unit of reactor output. Eagerly serialized so the runtime can
/// journal it without re-walking the type.
#[derive(Clone)]
pub struct EventOutput {
    pub type_id: TypeId,
    /// The event kind on the wire — `Event::NAME`, verbatim. Field
    /// name kept as `durable_name` for backend-impl compatibility.
    pub durable_name: String,
    /// `Event::SUBJECT` — the subject history this output joins
    /// (`{subject}-{subject_id}`). Defaults to `NAME`; differs when
    /// the event co-locates.
    pub subject: String,
    /// Stream id from `Event::subject_id()` — which stream within
    /// `subject` this output targets.
    pub subject_id: Uuid,
    /// `Some` = this fact roots its own workflow, named by its payload
    /// field (`Event::declared_workflow_id`). `None` = chain member;
    /// the runner stamps the trigger's workflow.
    pub workflow: Option<Uuid>,
    pub payload: serde_json::Value,
    /// Original typed fact (live dispatch only).
    pub ephemeral: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl EventOutput {
    /// Create from a typed Event. `durable_name` is `Event::NAME`,
    /// verbatim — the same string `Engine::emit` writes.
    pub fn new<F: crate::event::Event>(fact: F) -> Self {
        let subject = <F as crate::event::Event>::SUBJECT.to_string();
        let durable_name = <F as crate::event::Event>::NAME.to_string();
        let subject_id = fact.subject_id();
        let workflow = fact.declared_workflow_id();
        let payload = serde_json::to_value(&fact).expect("Event must be serializable");
        let ephemeral: Arc<dyn std::any::Any + Send + Sync> = Arc::new(fact);
        Self {
            type_id: TypeId::of::<F>(),
            durable_name,
            subject,
            subject_id,
            workflow,
            payload,
            ephemeral: Some(ephemeral),
        }
    }

    /// Reconstruct from a serialized form (replay path; no live
    /// ephemeral copy).
    pub fn from_serialized(
        event_type: String,
        subject_id: Uuid,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            type_id: TypeId::of::<()>(),
            // Replay/serialized path has no separate subject info;
            // default to the kind (SUBJECT's own default).
            subject: event_type.clone(),
            durable_name: event_type,
            subject_id,
            workflow: None,
            payload,
            ephemeral: None,
        }
    }
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
        const NAME: &'static str = "order_placed";
        fn subject_id(&self) -> Uuid { self.order_id }
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
            workflow_id: Uuid::nil(),
            metadata:       &meta,
            consumer: "",
            labels:   None,
            state:    crate::contexts::StateSource::None,
            logs:           None,
            effect_store: None,
            cancelled_workflows: None,
        };

        let events = r.react(&trigger, ctx).await.unwrap();
        assert_eq!(events.len(), 0);
    }
}
