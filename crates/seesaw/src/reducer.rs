//! Reducer - Pure state transformations
//!
//! Reducers are pure functions that transform state in response to events.
//! They run before effects and provide updated state to effect handlers.
//!
//! # Architecture
//!
//! ```text
//! Event → Reducers → Effects
//!          ↓
//!      state' = reduce(state, event)
//! ```
//!
//! # Example
//!
//! ```rust
//! use seesaw::{Reducer, Event};
//!
//! #[derive(Clone)]
//! struct RequestState {
//!     counter: i32,
//! }
//!
//! struct Increment {
//!     amount: i32,
//! }
//!
//! impl Event for Increment { /* ... */ }
//!
//! struct IncrementReducer;
//!
//! impl Reducer<Increment, RequestState> for IncrementReducer {
//!     fn reduce(&self, state: &RequestState, event: &Increment) -> RequestState {
//!         RequestState {
//!             counter: state.counter + event.amount,
//!         }
//!     }
//! }
//!
//! // Register with engine
//! let engine = EngineBuilder::new(deps)
//!     .with_reducer::<Increment, RequestState>(IncrementReducer)
//!     .build();
//! ```

use crate::core::Event;

/// Reducer trait - Pure state transformation
///
/// Reducers are pure functions that:
/// 1. Take current state and an event
/// 2. Return new state
/// 3. Have no side effects
/// 4. Are deterministic
///
/// Reducers run before effects to provide updated state.
pub trait Reducer<E: Event, S>: Send + Sync + 'static {
    /// Pure state transformation
    ///
    /// Takes the current state and event, returns new state.
    /// Must be pure - no side effects, no mutations, deterministic.
    fn reduce(&self, state: &S, event: &E) -> S;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Increment {
        amount: i32,
    }

    impl Event for Increment {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct CounterState {
        count: i32,
    }

    struct IncrementReducer;

    impl Reducer<Increment, CounterState> for IncrementReducer {
        fn reduce(&self, state: &CounterState, event: &Increment) -> CounterState {
            CounterState {
                count: state.count + event.amount,
            }
        }
    }

    #[test]
    fn test_reducer_pure_transformation() {
        let reducer = IncrementReducer;
        let state = CounterState { count: 10 };
        let event = Increment { amount: 5 };

        let new_state = reducer.reduce(&state, &event);

        assert_eq!(new_state.count, 15);
        // Original state unchanged
        assert_eq!(state.count, 10);
    }

    #[test]
    fn test_reducer_deterministic() {
        let reducer = IncrementReducer;
        let state = CounterState { count: 10 };
        let event = Increment { amount: 5 };

        let result1 = reducer.reduce(&state, &event);
        let result2 = reducer.reduce(&state, &event);

        assert_eq!(result1, result2);
    }

    #[test]
    fn test_reducer_multiple_applications() {
        let reducer = IncrementReducer;
        let mut state = CounterState { count: 0 };

        state = reducer.reduce(&state, &Increment { amount: 1 });
        state = reducer.reduce(&state, &Increment { amount: 2 });
        state = reducer.reduce(&state, &Increment { amount: 3 });

        assert_eq!(state.count, 6);
    }
}
