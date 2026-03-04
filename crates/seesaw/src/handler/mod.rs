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
//! // With init
//! store.with_handler(
//!     effect::on::<MyEvent>()
//!         .init(setup_handler)
//!         .then(handle_my_event)
//! );
//!
//! // on_any() — receives all events, can emit child events
//! store.with_handler(
//!     effect::on_any()
//!         .then(|event: AnyEvent, ctx| async move { Ok(events![]) })
//! );
//! ```

mod builders;
pub mod context;
mod error_event;
mod types;

pub use builders::{on, on_any, project, ProjectionBuilder, TransitionHandlerBuilder};
pub use context::Context;
pub use error_event::HandlerError;
pub use types::{
    AnyEvent, DlqTerminalInfo, Emit, ErrorContext, ErrorHandler, EventOutput, Events,
    GlobalDlqMapper, Handler, IntoEvents, JoinMode, Projection,
};
