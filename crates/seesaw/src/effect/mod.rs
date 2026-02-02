//! Effect system with builder API.
//!
//! Effects are side-effect handlers that react to events and state changes.
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::effect;
//!
//! // Single event
//! store.with_effect(effect::on::<MyEvent>().run(handle_my_event));
//!
//! // With setup
//! store.with_effect(
//!     effect::on::<MyEvent>()
//!         .started(setup_handler)
//!         .run(handle_my_event)
//! );
//!
//! // State transition
//! store.with_effect(
//!     effect::on_any()
//!         .transition(|prev, next| prev.status != next.status)
//!         .run(|ctx| async move { Ok(()) })
//! );
//!
//! // Group multiple effects
//! store.with_effect(effect::group([
//!     effect::on::<EventA>().run(handle_a),
//!     effect::on::<EventB>().run(handle_b),
//! ]));
//!
//! // Bridge to pipeline
//! store.with_effect(effect::bridge(|ctx| ctx.deps().valet.clone()));
//! ```

mod builders;
mod context;
mod types;

pub use builders::{bridge, group, on, on_any, task};
pub use context::EffectContext;
pub use types::{AnyEvent, Effect};

// Re-export for internal use
pub(crate) use context::{BoxedEmitter, EventEmitter, EventEnvelope};
