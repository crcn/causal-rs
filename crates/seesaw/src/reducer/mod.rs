//! Reducer system with builder API.
//!
//! Reducers are pure functions that transform state in response to events.
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::reducer;
//!
//! // Single event
//! store.with_reducer(reducer::on::<MyEvent>().run(|state, event| {
//!     State { count: state.count + event.amount, ..state }
//! }));
//!
//! // Group multiple reducers
//! store.with_reducer(reducer::group([
//!     reducer::on::<EventA>().run(reduce_a),
//!     reducer::on::<EventB>().run(reduce_b),
//! ]));
//! ```

mod builders;
mod types;

pub use builders::{group, on, on_any};
pub use types::{AnyEventRef, Reducer};
