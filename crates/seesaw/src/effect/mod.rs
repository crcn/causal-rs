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
//! // on_any() observer pattern
//! store.with_effect(
//!     effect::on_any()
//!         .then(|event, ctx| async move { Ok(()) })
//! );
//!
//! // Group multiple effects
//! store.with_effect(effect::group([
//!     effect::on::<EventA>().then(handle_a),
//!     effect::on::<EventB>().then(handle_b),
//! ]));
//!
//! ```

mod builders;
pub(crate) mod context;
mod error_event;
mod types;

pub use builders::{group, on, on_any};
pub use context::EffectContext;
pub use error_event::EffectError;
pub use types::{
    AnyEvent, DlqTerminalInfo, Effect, Emit, ErrorContext, ErrorHandler, EventOutput, JoinMode,
};
