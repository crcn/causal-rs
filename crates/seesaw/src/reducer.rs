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
//! ```rust,ignore
//! use seesaw_core::{Reducer, Event, EngineBuilder};
//!
//! #[derive(Clone)]
//! struct RequestState {
//!     counter: i32,
//! }
//!
//! #[derive(Clone)]
//! struct Increment {
//!     amount: i32,
//! }
//! // Event is auto-implemented
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
//! # struct Deps;
//! # let deps = Deps;
//! let engine = EngineBuilder::new(deps)
//!     .with_reducer::<Increment, RequestState>(IncrementReducer)
//!     .build();
//! ```

use std::any::TypeId;
use std::sync::Arc;

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

// =============================================================================
// Reducer Runner (Type-Erased)
// =============================================================================

/// Type-erased reducer runner that can handle any event type.
pub(crate) struct ReducerRunner<S: Clone> {
    event_type: TypeId,
    reduce_fn: Box<dyn Fn(&S, &dyn std::any::Any) -> S + Send + Sync>,
}

impl<S: Clone> ReducerRunner<S> {
    /// Create a reducer runner for a specific reducer and event type.
    pub fn new<E: Event + Clone, R: Reducer<E, S>>(reducer: R) -> Self {
        let reducer = Arc::new(reducer);

        Self {
            event_type: TypeId::of::<E>(),
            reduce_fn: Box::new(move |state, any_event| {
                // Downcast the event
                let Some(event) = any_event.downcast_ref::<E>() else {
                    // If downcast fails, return state unchanged
                    return (*state).clone();
                };

                // Apply reducer
                reducer.reduce(state, event)
            }),
        }
    }

    /// Get the event type this runner handles.
    pub fn event_type(&self) -> TypeId {
        self.event_type
    }

    /// Try to run the reducer if the event matches.
    /// Returns Some(new_state) if the event type matches, None otherwise.
    pub fn try_reduce(&self, state: &S, event: &dyn std::any::Any) -> Option<S> {
        if event.type_id() == self.event_type {
            Some((self.reduce_fn)(state, event))
        } else {
            None
        }
    }
}

impl<S: Clone> std::fmt::Debug for ReducerRunner<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReducerRunner")
            .finish_non_exhaustive()
    }
}

// =============================================================================
// Reducer Registry
// =============================================================================

/// Registry of reducers.
///
/// Reducers are registered per event type and applied to transform state
/// before effects run.
#[derive(Default)]
pub(crate) struct ReducerRegistry<S: Clone> {
    reducers: Vec<ReducerRunner<S>>,
}

impl<S: Clone> ReducerRegistry<S> {
    /// Create a new empty reducer registry.
    pub fn new() -> Self {
        Self {
            reducers: Vec::new(),
        }
    }

    /// Register a reducer for an event type.
    pub fn register<E: Event + Clone, R: Reducer<E, S>>(&mut self, reducer: R) {
        self.reducers.push(ReducerRunner::new(reducer));
    }

    /// Apply all matching reducers to transform the state.
    ///
    /// Reducers are applied in registration order. Each reducer that matches
    /// the event type transforms the state, with the output becoming input
    /// to the next reducer.
    pub fn reduce_all(&self, state: &S, event: &dyn std::any::Any) -> S {
        let mut current_state = state.clone();

        for reducer in &self.reducers {
            if let Some(new_state) = reducer.try_reduce(&current_state, event) {
                current_state = new_state;
            }
        }

        current_state
    }

    /// Check if any reducers are registered.
    pub fn is_empty(&self) -> bool {
        self.reducers.is_empty()
    }

    /// Get the number of registered reducers.
    pub fn len(&self) -> usize {
        self.reducers.len()
    }
}

impl<S: Clone> std::fmt::Debug for ReducerRegistry<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReducerRegistry")
            .field("reducer_count", &self.reducers.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Increment {
        amount: i32,
    }

    // Event is auto-implemented via blanket impl

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

    // Test ReducerRegistry
    #[test]
    fn test_reducer_registry_applies_matching_reducer() {
        let mut registry = ReducerRegistry::new();
        registry.register::<Increment, _>(IncrementReducer);

        let state = CounterState { count: 10 };
        let event = Increment { amount: 5 };

        let new_state = registry.reduce_all(&state, &event);

        assert_eq!(new_state.count, 15);
        // Original state unchanged
        assert_eq!(state.count, 10);
    }

    #[test]
    fn test_reducer_registry_chains_multiple_reducers() {
        #[derive(Clone, Debug, PartialEq)]
        struct Multiply {
            factor: i32,
        }

        struct MultiplyReducer;

        impl Reducer<Multiply, CounterState> for MultiplyReducer {
            fn reduce(&self, state: &CounterState, event: &Multiply) -> CounterState {
                CounterState {
                    count: state.count * event.factor,
                }
            }
        }

        let mut registry = ReducerRegistry::new();
        registry.register::<Increment, _>(IncrementReducer);
        registry.register::<Multiply, _>(MultiplyReducer);

        let state = CounterState { count: 10 };

        // Apply increment
        let event1 = Increment { amount: 5 };
        let state = registry.reduce_all(&state, &event1);
        assert_eq!(state.count, 15);

        // Apply multiply
        let event2 = Multiply { factor: 2 };
        let state = registry.reduce_all(&state, &event2);
        assert_eq!(state.count, 30);
    }

    #[test]
    fn test_reducer_registry_no_matching_reducer() {
        let registry = ReducerRegistry::<CounterState>::new();

        let state = CounterState { count: 10 };
        let event = Increment { amount: 5 };

        let new_state = registry.reduce_all(&state, &event);

        // State unchanged when no reducers match
        assert_eq!(new_state.count, 10);
    }

    #[test]
    fn test_reducer_registry_len_and_empty() {
        let mut registry = ReducerRegistry::<CounterState>::new();

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        registry.register::<Increment, _>(IncrementReducer);

        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }
}
