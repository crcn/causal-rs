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
//! let engine = Engine::new(deps, store)
//!     .with_handler(on::<UserCreated>().then(|event, _ctx| async move {
//!         println!("User created: {}", event.name);
//!         Ok(UserWelcomed { name: event.name.clone() })
//!     }));
//!
//! // Publish event into the runtime queue.
//! engine.process(UserCreated { name: "Alice".into() }).await?;
//! ```

extern crate self as seesaw_core;

// New module structure
pub mod dead_letter_queue;
pub mod distributed_safe;
pub mod handler;
pub mod insight;
pub mod queue_backend;
pub mod runtime;
pub mod store;

mod engine_v2;
mod event_codec;
mod handler_registry;
mod process;

// Re-export main types
pub use dead_letter_queue::{DeadLetter, DeadLetterQueue, DlqStats, DlqStatus, RetrySummary};
pub use distributed_safe::DistributedSafe;
pub use engine_v2::{Engine, EngineBackend};
pub use handler::{
    AnyEvent, Context, DlqTerminalInfo, Emit, ErrorContext, Handler, HandlerError, JoinMode,
};
pub use insight::{
    EventNode, HandlerNode, InsightEvent, InsightStats, InsightStore, StreamType, WorkflowTree,
};
pub use process::{ProcessFuture, ProcessHandle, WaitFuture};
pub use queue_backend::{QueueBackend, StoreQueueBackend};
pub use runtime::{Runtime, RuntimeConfig};
pub use store::{
    EmittedEvent, EventProcessingCommit, ExpiredJoinWindow, InlineHandlerFailure, JoinEntry,
    QueuedEvent, QueuedHandlerExecution, QueuedHandlerIntent, Store, WorkflowEvent, WorkflowStatus,
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
