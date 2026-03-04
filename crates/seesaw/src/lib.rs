//! # Seesaw
//!
//! A deterministic event/handler runtime with TypeId-based multi-event dispatch.
//!
//! ## Guarantees
//!
//! - **Multi-event dispatch**: Support for multiple event types via TypeId routing.
//! - **Handler system**: Register handlers that react to events and can emit
//!   new events and access shared dependencies.
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::{Engine, on};
//!
//! // Define event types (struct-per-event pattern)
//! #[derive(Clone)]
//! struct UserCreated { name: String }
//! #[derive(Clone)]
//! struct UserWelcomed { name: String }
//!
//! // Create engine with handlers
//! let engine = Engine::new(deps)
//!     .with_handler(on::<UserCreated>().then(|event, _ctx| async move {
//!         println!("User created: {}", event.name);
//!         Ok(UserWelcomed { name: event.name.clone() })
//!     }));
//!
//! // Emit + settle the full causal tree.
//! engine.emit(UserCreated { name: "Alice".into() }).settled().await?;
//! ```

extern crate self as seesaw_core;

// Module structure
pub mod aggregator;
pub mod event_store;
pub mod handler;
pub mod job_executor;
pub mod store;
pub mod types;

mod engine;
mod event_codec;
mod handler_registry;
pub mod memory_store;
mod process;
pub mod upcaster;

// Re-export aggregator types
pub use aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};

// Re-export event store helpers and types
pub use event_store::{event_type_short_name, persist_event, save_snapshot, Versioned};
pub use types::{NewEvent, PersistedEvent, Snapshot};

// Re-export store trait and in-memory implementation
pub use store::Store;
pub use memory_store::MemoryStore;

// Re-export upcaster types
pub use upcaster::{Upcaster, UpcasterRegistry};

// Re-export main types
pub use engine::Engine;
pub use handler::{
    AnyEvent, Cancellable, Context, DlqTerminalInfo, Emit, ErrorContext, EventOutput, Events,
    Handler, HandlerError, IntoEvents, JoinMode, Projection,
};

/// The universal return macro for all handlers.
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

pub use job_executor::{EffectResult, EffectStatus, JobExecutor, JoinClaim};
pub use process::{EmitFuture, ProcessHandle, SettleFuture};
pub use types::{
    EffectCompletion, EffectDlq, EffectIntent, EmittedEvent, EventCommit, EventWorkerConfig,
    ExpiredJoinWindow, HandlerWorkerConfig, InlineFailure, JoinAppendParams, JoinEntry,
    QueuedEffect, QueuedEvent, NAMESPACE_SEESAW,
};

// Top-level builder functions
pub use handler::{on, on_any, project};

#[cfg(feature = "macros")]
pub use seesaw_core_macros::{
    aggregator, aggregators, handle, handler, handlers, handles, projection,
};

