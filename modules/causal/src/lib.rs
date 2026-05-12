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
//! use causal::{Engine, on};
//!
//! // Define event types (struct-per-event pattern)
//! #[derive(Clone)]
//! struct UserCreated { name: String }
//! #[derive(Clone)]
//! struct UserWelcomed { name: String }
//!
//! // Create engine with reactors
//! let engine = Engine::in_memory(deps)
//!     .with_reactor(on::<UserCreated>().then(|event, _ctx| async move {
//!         println!("User created: {}", event.name);
//!         Ok(UserWelcomed { name: event.name.clone() })
//!     }));
//!
//! // Emit + settle the full causal tree.
//! engine.emit(UserCreated { name: "Alice".into() }).settled().await?;
//! ```

extern crate self as causal;

// Module structure
pub mod aggregate_v3;
pub mod aggregator;
pub mod checkpoint_store;
pub mod contexts;
pub mod engine_v3;
pub mod event;
pub mod event_log;
pub mod event_store;
pub mod fact;
pub mod multi_projector;
pub mod projection;
pub mod projection_runner;
pub mod projector;
pub mod reactor_queue;
pub mod reactor_runner;
pub mod reactor_v3;
pub mod relay;
pub mod snapshot_store;
pub mod types;

mod event_codec;
pub mod memory_store;
pub mod upcaster;

// ── v0.4 public surface ──────────────────────────────────────────────
//
// What's in the prelude vs. behind a module path follows one rule:
// **the prelude is for what a user types in normal application code**.
//
// User-facing (here): facts, aggregates, consumer traits, the engine,
// the emit builder, registration trait objects, context types,
// cursors/versions, backend traits (cast at builder time), MemoryStore,
// upcasters, macros.
//
// Backend-impl-facing (module paths only): NewEvent, AppendResult,
// Snapshot — backends pull these from `causal::types::*` directly.
// Same for runners (`causal::projection_runner::*`), outbox row shapes
// (`causal::checkpoint_store::*`), and the relay (`causal::relay::*`).
//
// Internal-detail (module paths only): EventOutput (Events
// implementation detail), EmitInput (Into-target only),
// AggregatorRegistry (engine-internal state), extract_prefix
// (replaced by PersistedEvent::category() for the common case).

// Facts + aggregates
pub use fact::Fact;
pub use aggregate_v3::{Aggregate, Apply};
pub use aggregator::Aggregator;

// Consumers
pub use projector::Projector;
pub use multi_projector::MultiProjector;
pub use reactor_v3::{Events, Reactor};

// Engine + emit
pub use engine_v3::{
    Engine, EngineBuilder,
    EmitBuilder, EmitError, EmitResult,
    MultiProjectorRegistration, ProjectorRegistration, ReactorRegistration,
};

// Context
pub use contexts::{AggregateState, Ctx, Metadata};

// Cursors / versions / values users see in signatures
pub use types::{LogCursor, PersistedEvent, StreamVersion};

// Backend traits (users typically cast `Arc<dyn ...>` at builder time)
pub use checkpoint_store::{CheckpointStore, ReactorOutbox};
pub use event_log::EventLogBackend;
pub use projection::{
    Backoff, FailureBehavior, ProjectionFailure, ProjectionMode, ProjectionOps,
    ProjectionStatus, RetryPolicy, StartPosition,
};
pub use snapshot_store::SnapshotStore;

// Default backend
pub use memory_store::MemoryStore;

// Schema migration helpers
pub use upcaster::{Upcaster, UpcasterRegistry};

// ── Legacy carryover ──────────────────────────────────────────────
// `Event` trait stays until P11.d migrates `Events::push<E: Event>`
// to take `F: Fact`. `ProjectionStore` and legacy `aggregator::Aggregate`/
// `Apply<E>` stay until P11.e. Re-exported here so the transition
// doesn't force users onto deep module paths before the migration.
pub use event::Event;
pub use projection::ProjectionStore;

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

// Legacy reactor builder fns (`on`, `on_any`, `project`) and the
// macros that depend on them (`#[reactor]`, `#[reactors]`,
// `#[projection]`, `#[aggregator]`) are gone with the legacy
// reactor module. `#[event]` and `#[aggregators]` (plural) survive
// — they emit v0.4-compatible code.
#[cfg(feature = "macros")]
pub use causal_core_macros::{aggregators, event};

