//! # Seesaw
//!
//! A deterministic event/effect runtime with TypeId-based multi-event dispatch.
//!
//! ## Guarantees
//!
//! - **Multi-event dispatch**: Support for multiple event types via TypeId routing.
//! - **Effect system**: Register handlers that react to events and can emit
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
//! // Create engine with effects
//! let engine = Engine::new(deps, store)
//!     .with_effect(on::<UserCreated>().then(|event, _ctx| async move {
//!         println!("User created: {}", event.name);
//!         Ok(UserWelcomed { name: event.name.clone() })
//!     }));
//!
//! // Publish event into the runtime queue.
//! engine.process(UserCreated { name: "Alice".into() }).await?;
//! ```

extern crate self as seesaw_core;

// New module structure
pub mod effect;
pub mod insight;
pub mod queue_backend;
pub mod runtime;
pub mod store;

mod effect_registry;
mod engine_v2;
mod event_codec;
mod process;

// Re-export main types
pub use effect::{
    AnyEvent, DlqTerminalInfo, Effect, EffectContext, EffectError, Emit, ErrorContext, JoinMode,
};
pub use engine_v2::Engine;
pub use insight::{
    EffectNode, EventNode, InsightEvent, InsightStats, InsightStore, StreamType, WorkflowTree,
};
pub use process::{ProcessFuture, ProcessHandle, WaitFuture};
pub use queue_backend::{QueueBackend, StoreQueueBackend};
pub use runtime::{Runtime, RuntimeConfig};
pub use store::{
    EmittedEvent, EventProcessingCommit, InlineEffectFailure, JoinEntry, QueuedEffectExecution,
    QueuedEffectIntent, QueuedEvent, Store, WorkflowEvent, WorkflowStatus, NAMESPACE_SEESAW,
};

// Top-level builder functions
pub use effect::{on, on_any};

#[cfg(feature = "macros")]
pub use seesaw_core_macros::{effect, effects};

// Re-export commonly used external types
pub use async_trait::async_trait;

// Re-export pipedream types for convenience
pub use pipedream::Relay;
