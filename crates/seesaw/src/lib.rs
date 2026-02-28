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
pub mod backend;
pub mod dead_letter_queue;
pub mod distributed_safe;
pub mod es;
pub mod handler;
pub mod handler_runner;
pub mod insight;
pub mod queue_backend;
pub mod runtime;
pub mod store;

mod engine_v2;
mod event_codec;
mod handler_registry;
mod process;

// Re-export ES types
pub use es::{
    Aggregate, AggregateLoader, ConcurrencyError, EventStore, EventStoreExt, EventUpcast,
    HasEventStore, NewEvent, StoredEvent as EsStoredEvent, Versioned,
};

// Re-export main types
pub use backend::{Backend, BackendServeConfig, DispatchedEvent};
pub use backend::capability::{
    DeadLetter as DlqDeadLetter, DeadLetterQueueBackend, DlqFilters, InsightBackend, InsightStats as BackendInsightStats,
    SettleableBackend, TreeNode, WorkflowStatusBackend, WorkflowSubscriptionBackend, WorkflowTree as BackendWorkflowTree,
};
pub use backend::job_executor::{
    EventProcessingCommit as JobEventProcessingCommit, HandlerExecutionResult, HandlerStatus,
    InlineHandlerFailure as JobInlineHandlerFailure, JobExecutor,
};
pub use dead_letter_queue::{DeadLetter, DlqStats, DlqStatus, RetrySummary};
pub use distributed_safe::DistributedSafe;
pub use engine_v2::Engine;
pub use handler::{
    AnyEvent, Context, DlqTerminalInfo, Emit, ErrorContext, Handler, HandlerError, JoinMode,
};
pub use handler::es_projector::es_projector;
pub use handler_runner::{DirectRunner, HandlerRunner};
pub use insight::{
    EventNode, HandlerNode, InsightEvent, InsightStats, InsightStore, StreamType, WorkflowTree,
};
pub use process::{DispatchFuture, ProcessFuture, ProcessHandle, SettleFuture, WaitFuture};
pub use runtime::Runtime;
pub use store::{
    EmittedEvent, EventProcessingCommit, ExpiredJoinWindow, InlineHandlerFailure, JoinEntry,
    QueuedEvent, QueuedHandlerExecution, QueuedHandlerIntent, Store, WorkflowEvent, WorkflowStatus,
    NAMESPACE_SEESAW,
};

// Legacy exports (still needed internally by examples/postgres backend)
pub use queue_backend::{QueueBackend, StoreQueueBackend};
pub use runtime::event_worker::EventWorkerConfig;
pub use runtime::handler_worker::HandlerWorkerConfig;

// Top-level builder functions
pub use handler::{on, on_any};

#[cfg(feature = "macros")]
pub use seesaw_core_macros::{handle, handler, handlers, handles};

// Re-export commonly used external types
pub use async_trait::async_trait;

// Re-export pipedream types for convenience
pub use pipedream::Relay;
