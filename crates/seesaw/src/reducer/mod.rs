//! Reducer system with builder API.
//!
//! Reducers are pure functions that transform state in response to events.
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::reducer;
//!
//! // Fold event into state
//! store.with_reducer(fold::<MyEvent>().into(|state, event| {
//!     State { count: state.count + event.amount, ..state }
//! }));
//!
//! // Group multiple reducers
//! store.with_reducer(reducer::group([
//!     fold::<EventA>().into(reduce_a),
//!     fold::<EventB>().into(reduce_b),
//! ]));
//! ```

mod builders;
mod types;

pub use builders::{fold, group, on_any, FoldBuilder};
pub use types::{AnyEventRef, Reducer};
