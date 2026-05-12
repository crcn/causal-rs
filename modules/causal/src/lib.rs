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
pub mod reactor;
pub mod reactor_queue;
pub mod reactor_runner;
pub mod reactor_v3;
pub mod relay;
pub mod job_executor;
pub mod snapshot_store;
pub mod types;

mod engine;
mod event_codec;
mod reactor_registry;
pub mod memory_store;
mod process;
pub mod upcaster;

// Re-export Event trait
pub use event::Event;

// Re-export aggregator types
pub use aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};

// Re-export event store helpers and types
pub use event_store::{event_type_short_name, persist_event, save_snapshot, Versioned};
pub use types::{AppendResult, LogCursor, NewEvent, PersistedEvent, QueueStatus, Snapshot, StreamVersion};

// Re-export v0.3 trait surface
pub use checkpoint_store::{CheckpointStore, InsertableOutboxRow, OutboxRow, ReactorOutbox};
pub use contexts::{Ctx, Metadata};
pub use event_log::{EventLog, EventLogBackend};
pub use fact::Fact;
pub use multi_projector::{MultiProjector, MultiProjectorRunner};
pub use projection_runner::{ProjectionRunner, StepOutcome};
pub use projector::Projector;
pub use reactor_queue::ReactorQueue;
pub use reactor_runner::{derive_output_event_id, ReactorRunner, NS_REACTOR_OUTPUT};
pub use relay::RelayLoop;
// Note: `crate::engine::Engine<D>` is the legacy engine. The new v0.3
// engine is `crate::engine_v3::{Engine, EngineBuilder}` — fully-
// qualified to avoid the name collision until Phase 9.
pub use engine_v3::{
    EmitBuilder, EmitError, EmitInput, EmitResult,
    MultiProjectorRegistration, ProjectorRegistration, ReactorRegistration,
};
// Note: `crate::aggregator::Aggregate` is the legacy trait. The new
// v0.3 trait lives at `crate::aggregate_v3::Aggregate`.
pub use snapshot_store::SnapshotStore;
// Note: `crate::reactor::Reactor` is the legacy builder struct.
// The new v0.3 trait is `crate::reactor_v3::Reactor` — accessed by
// fully-qualified path to avoid the name collision until Phase 9.
pub use types::{EventPark, IntentCommit};

// Re-export projection configuration types and persistence trait.
// The runtime that uses these (engine integration, ProjectionRunner) is
// not yet implemented — see
// `docs/plans/2026-05-04-feat-async-projections-plan.md`.
pub use projection::{
    Backoff, FailureBehavior, ProjectionFailure, ProjectionMode, ProjectionOps,
    ProjectionStatus, ProjectionStore, RetryPolicy, StartPosition,
};

// Re-export in-memory implementation
pub use memory_store::MemoryStore;

// Re-export upcaster types
pub use upcaster::{Upcaster, UpcasterRegistry};

// Re-export main types
pub use engine::Engine;
pub use reactor::{
    AggregateState, AnyEvent, Context, DlqTerminalInfo, Emit, ErrorContext, EventOutput, Events,
    Reactor, ReactorError, IntoEvents, Logger, Projection,
};

/// The universal return macro for all reactors.
///
/// Works like `vec![]` — constructs an [`Events`] collection:
///
/// ```ignore
/// // Nothing — no events emitted
/// Ok(events![])
///
/// // Single event
/// Ok(events![OrderShipped { order_id }])
///
/// // Multiple heterogeneous events
/// Ok(events![ScrapeEvent { data }, LifecycleEvent::PhaseCompleted])
///
/// // Fan-out batch (each element becomes a separate event with batch metadata)
/// Ok(events![..items])
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

pub use job_executor::{ReactorResult, ReactorStatus, JobExecutor};
pub use process::{EmitFuture, ProcessHandle, SettleFuture};
pub use types::{
    EmittedEvent, EventWorkerConfig, ReactorCompletion,
    ReactorDlq, ReactorIntent, ReactorResolution, ReactorWorkerConfig,
    JournalEntry, LogEntry, LogLevel, QueuedReactor,
    NAMESPACE_CAUSAL,
};

// Top-level builder functions
pub use reactor::{on, on_any, project};

#[cfg(feature = "macros")]
pub use causal_core_macros::{
    aggregator, aggregators, event, projection, reactor, reactors,
};

