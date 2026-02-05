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

// New module structure
pub mod effect;
pub mod reducer;

mod effect_registry;
mod reducer_registry;
mod engine;
mod task_group;

// Service layer (action execution)
pub mod service;

// OpenTelemetry integration
#[cfg(feature = "otel")]
pub mod otel;

// Re-export main types
pub use effect::{AnyEvent, Effect, EffectContext, EffectError};
pub use reducer::Reducer;
pub use engine::{Handle, Engine};
pub use task_group::TaskGroup;

// Top-level builder functions
pub use effect::on;
pub use reducer::fold;

// Re-export service types at root level
pub use service::{
    ActionResult, ActionWithOpts, EmptyResult, GenericActionResult, IntoAction, Service,
    ServiceError,
};

// Re-export service submodules for backwards compatibility (use seesaw::action::...)
pub use service::action;
pub use service::into_action;

// Re-export commonly used external types
pub use async_trait::async_trait;

// Re-export pipedream types for convenience
pub use pipedream::Relay;
