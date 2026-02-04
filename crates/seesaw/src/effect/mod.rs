//! Effect system with builder API.
//!
//! Effects are side-effect handlers that react to events and optionally return new events.
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::effect;
//!
//! // Typed effect that returns a new event
//! store.with_effect(effect::on::<MyEvent>().then(|event, ctx| async move {
//!     ctx.deps().process(&event).await?;
//!     Ok(EventProcessed { id: event.id })
//! }));
//!
//! // Typed effect as observer (returns ())
//! store.with_effect(effect::on::<MyEvent>().then(|event, ctx| async move {
//!     ctx.deps().log(&event).await?;
//!     Ok(())
//! }));
//!
//! // With setup
//! store.with_effect(
//!     effect::on::<MyEvent>()
//!         .started(setup_handler)
//!         .then(handle_my_event)
//! );
//!
//! // on_any() observer pattern (uses .run())
//! store.with_effect(
//!     effect::on_any()
//!         .transition(|prev, next| prev.status != next.status)
//!         .run(|event, ctx| async move { Ok(()) })
//! );
//!
//! // Group multiple effects
//! store.with_effect(effect::group([
//!     effect::on::<EventA>().then(handle_a),
//!     effect::on::<EventB>().then(handle_b),
//! ]));
//!
//! // on! macro for multi-variant matching
//! use seesaw::on;
//!
//! let effects = on!(MyEvent {
//!     VariantA { id, data, .. } |
//!     VariantB { id, data, .. } => |ctx| async move {
//!         Ok(MyEvent::Processed { id })
//!     },
//!     VariantC { id, .. } => |ctx| async move {
//!         Ok(MyEvent::Handled { id })
//!     },
//! });
//! ```

mod builders;
pub(crate) mod context;
mod macros;
mod types;

pub use builders::{bridge, group, on, on_any, task};
pub use context::EffectContext;
pub use types::{AnyEvent, Effect, EventOutput};

// Re-export for internal use
pub(crate) use context::{BoxedEmitter, EventEmitter, EventEnvelope};
