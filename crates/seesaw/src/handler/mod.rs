//! Effect system with builder API.
//!
//! Effects are side-effect handlers that react to events and optionally return new events.
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::handler;
//!
//! // Typed effect that returns a new event
//! store.with_handler(effect::on::<MyEvent>().then(|event, ctx| async move {
//!     ctx.deps().process(&event).await?;
//!     Ok(EventProcessed { id: event.id })
//! }));
//!
//! // Typed effect as observer (returns ())
//! store.with_handler(effect::on::<MyEvent>().then(|event, ctx| async move {
//!     ctx.deps().log(&event).await?;
//!     Ok(())
//! }));
//!
//! // With setup
//! store.with_handler(
//!     effect::on::<MyEvent>()
//!         .started(setup_handler)
//!         .then(handle_my_event)
//! );
//!
//! // on_any() observer pattern
//! store.with_handler(
//!     effect::on_any()
//!         .then(|event, ctx| async move { Ok(()) })
//! );
//!
//! // Group multiple effects
//! store.with_handler(effect::group([
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
pub use context::HandlerContext;
pub use error_event::HandlerError;
pub use types::{
    AnyEvent, DlqTerminalInfo, Emit, ErrorContext, ErrorHandler, EventOutput, Handler, JoinMode,
};
