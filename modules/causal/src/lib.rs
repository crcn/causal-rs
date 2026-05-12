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

// Re-export Event trait
// Re-export legacy Event trait — used by `Events::push<E: Event>`
// until P11.d migrates that API to take `F: Fact`. Stays for now.
pub use event::Event;

// Aggregator dispatch + v0.4 Aggregate marker. Legacy `Aggregate`
// trait (with `aggregate_type()` method) and legacy `Apply<E>`
// (owned) are gone in P11.e; the trait surface here is the v0.4
// shape from `aggregate_v3`.
pub use aggregate_v3::{Aggregate, Apply};
pub use aggregator::{Aggregator, AggregatorRegistry};

pub use types::{AppendResult, LogCursor, NewEvent, PersistedEvent, Snapshot, StreamVersion};

// ── v0.4 public trait surface ─────────────────────────────────────────
// Implementation-detail runners + outbox shapes live behind module
// paths (causal::projection_runner::ProjectionRunner,
// causal::checkpoint_store::OutboxRow, etc.) so they don't crowd the
// prelude. Backends that implement ReactorOutbox import OutboxRow /
// InsertableOutboxRow from their module path directly.
pub use checkpoint_store::{CheckpointStore, ReactorOutbox};
pub use contexts::{AggregateState, Ctx, Metadata};
pub use event_log::EventLogBackend;
pub use fact::Fact;
pub use multi_projector::MultiProjector;
pub use projector::Projector;
pub use engine_v3::{
    EmitBuilder, EmitError, EmitInput, EmitResult,
    MultiProjectorRegistration, ProjectorRegistration, ReactorRegistration,
};
pub use snapshot_store::SnapshotStore;
pub use reactor_v3::{EventOutput, Events, Reactor, extract_prefix};

// Projection configuration types
pub use projection::{
    Backoff, FailureBehavior, ProjectionFailure, ProjectionMode, ProjectionOps,
    ProjectionStatus, ProjectionStore, RetryPolicy, StartPosition,
};

pub use memory_store::MemoryStore;

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

// Legacy reactor builder fns (`on`, `on_any`, `project`) and the
// macros that depend on them (`#[reactor]`, `#[reactors]`,
// `#[projection]`, `#[aggregator]`) are gone with the legacy
// reactor module. `#[event]` and `#[aggregators]` (plural) survive
// — they emit v0.4-compatible code.
#[cfg(feature = "macros")]
pub use causal_core_macros::{aggregators, event};

