//! # Causal
//!
//! Event-sourcing runtime for Rust, [KurrentDB](https://www.kurrent.io/)-aligned:
//! a typed `Event → Reactor → Event` loop over an append-only event log.
//! Runs entirely in-memory for tests ([`MemoryStore`]); production backends
//! (KurrentDB event log, Postgres cursors/snapshots) live in `causal_replay`.
//!
//! The vocabulary mirrors KurrentDB's where the concepts overlap:
//! [`EventData`](types::EventData) / [`RecordedEvent`], `causation_id` /
//! `workflow_id`, [`StreamRevision`] (0-indexed), [`StreamState`] for
//! optimistic concurrency, `{category}-{subject_id}` stream names.
//!
//! ## Guarantees
//!
//! - **At-least-once delivery, idempotent appends** — reactor outputs carry
//!   deterministic `event_id`s, so crash-redelivery dedups on append.
//! - **Replay determinism** — no wall-clock inside consumer bodies;
//!   aggregate state is a pure fold of the log.
//! - **Quiescence** — `emit(...).settled()` resolves when the event's
//!   workflow chain has been observed by every consumer.
//! - **OCC where you opt in** — the `Engine::append` decider path enforces
//!   [`StreamState`] expectations with typed conflicts.
//!
//! ## Example
//!
//! ```
//! use causal::{Ctx, EngineBuilder, Event, Events, Reactor};
//! use serde::{Deserialize, Serialize};
//! use uuid::Uuid;
//!
//! #[derive(Debug, Clone, Serialize, Deserialize)]
//! struct OrderPlaced { order_id: Uuid, total: f64 }
//!
//! impl Event for OrderPlaced {
//!     const NAME: &'static str = "order_placed";
//!     fn subject_id(&self) -> Uuid { self.order_id }
//! }
//!
//! #[derive(Debug, Clone, Serialize, Deserialize)]
//! struct ShipmentRequested { order_id: Uuid }
//!
//! impl Event for ShipmentRequested {
//!     const NAME: &'static str = "shipment_requested";
//!     fn subject_id(&self) -> Uuid { self.order_id }
//! }
//!
//! struct ShipOnPlaced;
//!
//! #[async_trait::async_trait]
//! impl Reactor for ShipOnPlaced {
//!     type Trigger = OrderPlaced;
//!     const NAME: &'static str = "ship_on_placed";
//!
//!     async fn react(&self, t: &OrderPlaced, _ctx: Ctx<'_>) -> anyhow::Result<Events> {
//!         Ok(causal::events![ShipmentRequested { order_id: t.order_id }])
//!     }
//! }
//!
//! # fn main() -> anyhow::Result<()> {
//! # tokio::runtime::Runtime::new()?.block_on(async {
//! let engine = EngineBuilder::memory()
//!     .with_reactor(ShipOnPlaced)
//!     .build()
//!     .await?;
//!
//! // Persists the event, stamps causation/workflow, and resolves once
//! // ShipOnPlaced has reacted and its output has landed in the log.
//! engine.emit(OrderPlaced { order_id: Uuid::new_v4(), total: 99.99 })
//!     .settled()
//!     .await?;
//! # anyhow::Ok(())
//! # })
//! # }
//! ```
//!
//! The `#[event]` macro (feature `macros`, on by default) generates the
//! [`Event`] impl from a struct declaration
//! (`#[event(name = "order_placed", subject_id = "order_id")]`); the
//! example hand-rolls it to show the full trait surface.

extern crate self as causal;

// Module structure
pub mod aggregate;
pub mod aggregator;
pub mod checkpoint_store;
pub mod clock;
pub mod contexts;
pub mod engine;
pub mod event_log;
pub mod event_type;
pub mod event;
pub mod failure;
pub mod multi_projector;
pub mod projection;
pub mod projection_failure;
pub mod projection_runner;
pub mod projector;
pub mod effect_store;
pub mod decision_store;
pub mod reactor_observer;
pub mod reactor_runner;
pub mod reactor;
pub mod snapshot_store;
pub mod types;

pub mod consumer_lease;
pub mod memory_store;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

// ── public surface ──────────────────────────────────────────────
//
// What's in the prelude vs. behind a module path follows one rule:
// **the prelude is for what a user types in normal application code**.
//
// User-facing (here): facts, aggregates, consumer traits, the engine,
// the emit builder, registration trait objects, context types,
// cursors/versions, backend traits (cast at builder time), MemoryStore,
// macros.
//
// Backend-impl-facing (module paths only): EventData, WriteResult,
// Snapshot — backends pull these from `causal::types::*` directly.
// Same for runners (`causal::projection_runner::*`) and the
// checkpoint / reactor-checkpoint traits (`causal::checkpoint_store::*`).
//
// Internal-detail (module paths only): EventOutput (Events
// implementation detail), EmitInput (Into-target only),
// AggregatorRegistry (engine-internal state), extract_prefix
// (replaced by RecordedEvent::category() for the common case).

// Facts + aggregates
pub use event::{event_type_for, stream_name_for, Event};
pub use aggregate::{Aggregate, Apply};
pub use aggregator::Aggregator;

// Consumers
pub use projector::Projector;
pub use multi_projector::MultiProjector;
pub use reactor::{Events, Ordering, Reactor, RetryPolicy};
pub use failure::{domain, poison, transient, ErrorClass, FailureClass};

// Engine + emit
pub use engine::{
    append_workflow_cancelled,
    Boundary,
    DrainResult, RunnerConfig,
    Engine, EngineBuilder,
    EmitBuilder, EmitResult, SettledEmit,
    TerminalFailure,
    MultiProjectorRegistration, ProjectorRegistration, ReactorRegistration,
};

// Context
pub use contexts::{AggregateState, Ctx, Metadata};

// Cursors / revisions / values users see in signatures
pub use types::{
    WriteResult, EventData, LogCursor, LogEntry, LogLevel, RecordedEvent,
    StreamRevision, StreamState,
};

// Observability hook
pub use reactor_observer::{NoopObserver, ReactorObserver};

// Reactor idempotency (Phase 4 foundation): memoize side-effecting
// reactions so at-least-once delivery is safe; deterministic emit ids.
pub use effect_store::{remember, InMemoryEffectStore, EffectStore, EffectKey};
pub use decision_store::{DecisionRecord, DecisionStore, InMemoryDecisionStore};

// Consumer lease: exclusive per-consumer claim preventing two servers from
// processing the same consumer's triggers simultaneously.
pub use consumer_lease::{ConsumerLeasor, LeaseGuard};

// Backend traits (users typically cast `Arc<dyn ...>` at builder time)
pub use checkpoint_store::{CheckpointStore, ReactorCheckpoint};
pub use clock::{Clock, FixedClock, SystemClock};
pub use event_log::{append_event, EventLogBackend};
pub use projection::StartPosition;
pub use snapshot_store::SnapshotStore;

// Default backend
pub use memory_store::MemoryStore;

// Re-exported for macro-generated consumer impls (#[causal::reactors]
// / #[causal::projectors] emit `#[::causal::async_trait]` impls).
pub use async_trait::async_trait;


/// Universal return macro for [`Reactor::react`]. Builds an
/// [`Events`] collection from one or more output facts.
///
/// ```ignore
/// Ok(events![])                                  // no outputs
/// Ok(events![OrderShipped { order_id }])         // one output
/// Ok(events![ScrapeEvent { .. }, Lifecycle::..]) // many
/// Ok(events![..items])                           // fan-out batch
/// ```
#[macro_export]
macro_rules! events {
    () => {
        $crate::Events::new()
    };
    (.. $spread:expr) => {
        $crate::Events::batch($spread)
    };
    ($single:expr $(,)?) => {{
        let mut __ev = $crate::Events::new();
        __ev.push($single);
        __ev
    }};
    ($($event:expr),+ $(,)?) => {{
        let mut __ev = $crate::Events::new();
        $(__ev.push($event);)+
        __ev
    }};
}

#[cfg(feature = "macros")]
pub use causal_core_macros::{aggregators, event, projectors, reactors};

