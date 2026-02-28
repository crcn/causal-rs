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
//! // Dispatch + settle the full causal tree.
//! engine.dispatch(UserCreated { name: "Alice".into() }).settled().await?;
//! ```

extern crate self as seesaw_core;

// Module structure
pub mod es;
pub mod handler;
pub mod handler_runner;
pub mod insight;
pub mod job_executor;
pub mod types;

mod engine_v2;
mod event_codec;
mod handler_registry;
#[allow(dead_code)]
pub(crate) mod memory_store;
mod process;

// Re-export ES types
pub use es::{
    Aggregate, AggregateLoader, ConcurrencyError, EventStore, EventStoreExt, EventUpcast,
    HasEventStore, NewEvent, StoredEvent as EsStoredEvent, Versioned,
};

// Re-export main types
pub use engine_v2::Engine;
pub use handler::{
    AnyEvent, Context, DlqTerminalInfo, Emit, ErrorContext, Handler, HandlerContext, HandlerError,
    JoinMode,
};
pub use handler::es_projector::es_projector;
pub use handler_runner::{DirectRunner, HandlerRunner};
pub use insight::{
    EventNode, HandlerNode, InsightEvent, InsightStats, InsightStore, StreamType, WorkflowTree,
};
pub use job_executor::{
    EventProcessingCommit as JobEventProcessingCommit, HandlerExecutionResult, HandlerStatus,
    InlineHandlerFailure as JobInlineHandlerFailure, JobExecutor, JoinClaim,
};
pub use process::{DispatchFuture, ProcessHandle, SettleFuture};
pub use types::{
    EmittedEvent, EventProcessingCommit, EventWorkerConfig, ExpiredJoinWindow,
    HandlerWorkerConfig, InlineHandlerFailure, JoinEntry, QueuedEvent,
    QueuedHandlerExecution, QueuedHandlerIntent, WorkflowEvent, WorkflowStatus,
    NAMESPACE_SEESAW,
};

// Top-level builder functions
pub use handler::{on, on_any};

#[cfg(feature = "macros")]
pub use seesaw_core_macros::{handle, handler, handlers, handles};

// Re-export commonly used external types
pub use async_trait::async_trait;

// Re-export pipedream types for convenience
pub use pipedream::Relay;
