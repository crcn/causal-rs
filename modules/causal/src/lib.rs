//! # Causal
//!
//! A deterministic event/reactor runtime with TypeId-based multi-event dispatch.
//!
//! ## Guarantees
//!
//! - **Multi-event dispatch**: Support for multiple event types via TypeId routing.
//! - **Reactor system**: Register reactors that react to events and can emit
//!   new events and access shared dependencies.
//!
//! ## Example
//!
//! ```ignore
//! use causal::{Engine, Reactor, Events, Ctx, MemoryStore};
//! use std::sync::Arc;
//!
//! struct WelcomeOnSignup;
//!
//! #[async_trait::async_trait]
//! impl Reactor for WelcomeOnSignup {
//!     type Trigger = UserCreated;
//!     const GROUP_NAME: &'static str = "welcome_on_signup";
//!
//!     async fn react(&self, trigger: &UserCreated, _ctx: Ctx<'_>) -> anyhow::Result<Events> {
//!         Ok(causal::events![UserWelcomed { name: trigger.name.clone() }])
//!     }
//! }
//!
//! let store = Arc::new(MemoryStore::new());
//! let engine = Engine::builder(store.clone(), store.clone(), store)
//!     .with_reactor(WelcomeOnSignup)
//!     .build();
//!
//! engine.emit(UserCreated { name: "Alice".into() }).settled().await?;
//! ```

extern crate self as causal;

// Module structure
pub mod aggregate;
pub mod aggregator;
pub mod checkpoint_store;
pub mod contexts;
pub mod engine;
pub mod event_log;
pub mod event;
pub mod multi_projector;
pub mod projection;
pub mod projection_runner;
pub mod projector;
pub mod reaction_cache;
pub mod reactor_observer;
pub mod reactor_runner;
pub mod reactor;
pub mod snapshot_store;
pub mod types;

pub mod memory_store;
pub mod upcaster;

// ── public surface ──────────────────────────────────────────────
//
// What's in the prelude vs. behind a module path follows one rule:
// **the prelude is for what a user types in normal application code**.
//
// User-facing (here): facts, aggregates, consumer traits, the engine,
// the emit builder, registration trait objects, context types,
// cursors/versions, backend traits (cast at builder time), MemoryStore,
// upcasters, macros.
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
pub use reactor::{Events, Reactor};

// Engine + emit
pub use engine::{
    Engine, EngineBuilder,
    EmitBuilder, EmitResult, SettledEmit,
    DlqInfo,
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
pub use reaction_cache::{remember, InMemoryReactionCache, ReactionCache, ReactionKey};

// Backend traits (users typically cast `Arc<dyn ...>` at builder time)
pub use checkpoint_store::{CheckpointStore, ReactorCheckpoint};
pub use event_log::{append_event, EventLogBackend};
pub use projection::{
    Backoff, FailureBehavior, ProjectionFailure, ProjectionMode, ProjectionOps,
    ProjectionStatus, RetryPolicy, StartPosition,
};
pub use snapshot_store::SnapshotStore;

// Default backend
pub use memory_store::MemoryStore;

// Schema migration helpers
pub use upcaster::{Upcaster, UpcasterRegistry};


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
pub use causal_core_macros::{aggregators, event};

