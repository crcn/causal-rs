//! # Seesaw
//!
//! A Redux-style state machine with TypeId-based multi-event dispatch.
//!
//! Named after the playground equipment that balances back and forth —
//! representing the back-and-forth nature of state transitions.
//!
//! ## Guarantees
//!
//! - **Serial reduction**: Reducers are executed serially under a write lock.
//!   No two events are ever reduced concurrently, even when emitted from
//!   multiple tasks.
//! - **Multi-event dispatch**: Support for multiple event types via TypeId routing.
//! - **Effect system**: Register handlers that react to events and can emit
//!   new events, spawn tracked tasks, and access shared dependencies.
//! - **Per-event reducers**: Register reducers for specific event types.
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::{Engine, on, fold};
//!
//! #[derive(Clone, Debug, Default)]
//! struct AppState {
//!     user_count: i32,
//!     order_count: i32,
//! }
//!
//! // Define event types (struct-per-event pattern)
//! #[derive(Clone)]
//! struct UserCreated { name: String }
//! #[derive(Clone)]
//! struct OrderPlaced { amount: f64 }
//! #[derive(Clone)]
//! struct UserWelcomed { name: String }
//!
//! // Create engine with fold (reducers) and on (effects)
//! let engine = Engine::new()
//!     // Fold events into state
//!     .with_reducer(fold::<UserCreated>().into(|state, _| AppState {
//!         user_count: state.user_count + 1,
//!         ..state
//!     }))
//!     .with_reducer(fold::<OrderPlaced>().into(|state, _| AppState {
//!         order_count: state.order_count + 1,
//!         ..state
//!     }))
//!     // On event, then return next event
//!     .with_effect(on::<UserCreated>().then(|event, _ctx| async move {
//!         println!("User created: {}", event.name);
//!         Ok(UserWelcomed { name: event.name.clone() })
//!     }));
//!
//! // Activate and dispatch events via process()
//! let handle = engine.activate(AppState::default());
//! handle.process(|_| async { Ok(UserCreated { name: "Alice".into() }) }).await?;
//! handle.process(|_| async { Ok(OrderPlaced { amount: 99.99 }) }).await?;
//! ```

extern crate self as seesaw_core;

// New module structure
pub mod effect;
pub mod insight;
pub mod reducer;
pub mod runtime;
pub mod store;

mod effect_registry;
mod engine_v2;
mod event_codec;
mod process;
mod reducer_registry;
mod task_group;

// Re-export main types
pub use effect::{
    AnyEvent, DlqTerminalInfo, Effect, EffectContext, EffectError, Emit, ErrorContext, JoinMode,
};
pub use engine_v2::Engine;
pub use insight::{
    EffectNode, EventNode, InsightEvent, InsightStats, InsightStore, StreamType, WorkflowTree,
};
pub use process::{ProcessFuture, ProcessHandle, WaitFuture};
pub use reducer::Reducer;
pub use runtime::{Runtime, RuntimeConfig};
pub use store::{
    EmittedEvent, JoinEntry, QueuedEffectExecution, QueuedEvent, Store, WorkflowEvent,
    WorkflowStatus, NAMESPACE_SEESAW,
};
pub use task_group::TaskGroup;

// Top-level builder functions
pub use effect::on;
pub use reducer::fold;

#[cfg(feature = "macros")]
pub use seesaw_macros::{effect, effects, reducer, reducers};

// Re-export commonly used external types
pub use async_trait::async_trait;

// Re-export pipedream types for convenience
pub use pipedream::Relay;
