//! Reactor system with builder API.
//!
//! Reactors react to events and optionally return new events.
//!
//! ## Example
//!
//! ```ignore
//! use causal::reactor;
//!
//! // Typed reactor that returns a new event
//! engine.with_reactor(reactor::on::<MyEvent>().then(|event, ctx| async move {
//!     ctx.deps().process(&event).await?;
//!     Ok(EventProcessed { id: event.id })
//! }));
//!
//! // Typed reactor as observer (returns ())
//! engine.with_reactor(reactor::on::<MyEvent>().then(|event, ctx| async move {
//!     ctx.deps().log(&event).await?;
//!     Ok(())
//! }));
//!
//! // With init
//! engine.with_reactor(
//!     reactor::on::<MyEvent>()
//!         .init(setup_handler)
//!         .then(handle_my_event)
//! );
//!
//! // on_any() — receives all events, can emit child events
//! engine.with_reactor(
//!     reactor::on_any()
//!         .then(|event: AnyEvent, ctx| async move { Ok(events![]) })
//! );
//! ```

mod builders;
pub mod context;
mod error_event;
mod types;

pub use builders::{on, on_any, project, FilteredReactorBuilder, ProjectionBuilder, TransitionReactorBuilder};
pub use context::{AggregateState, Context, Logger};
pub use error_event::ReactorError;
pub use types::{
    extract_prefix, AnyEvent, DlqTerminalInfo, Emit, ErrorContext, ErrorHandler, EventOutput,
    Events, GlobalDlqMapper, Reactor, IntoEvents, Projection,
};
