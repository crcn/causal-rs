//! Reducer registry for TypeId-based state reduction.

use std::any::{Any, TypeId};

use parking_lot::RwLock;

use crate::reducer::Reducer;

/// Registry for storing and applying reducers.
pub(crate) struct ReducerRegistry<S>
where
    S: Clone + Send + Sync + 'static,
{
    reducers: RwLock<Vec<Reducer<S>>>,
}

impl<S> Default for ReducerRegistry<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S> ReducerRegistry<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Create a new empty reducer registry.
    pub fn new() -> Self {
        Self {
            reducers: RwLock::new(Vec::new()),
        }
    }

    /// Register a reducer.
    pub fn register(&self, reducer: Reducer<S>) {
        self.reducers.write().push(reducer);
    }

    /// Apply all matching reducers for the given event type.
    ///
    /// Reducers are applied in order. Each reducer receives the state
    /// produced by the previous reducer.
    pub fn apply(&self, state: S, type_id: TypeId, event: &dyn Any) -> S {
        let mut state = state;
        for reducer in self.reducers.read().iter() {
            if reducer.can_handle(type_id) {
                state = reducer.apply(state, event, type_id);
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reducer;

    #[derive(Debug, Clone, PartialEq)]
    struct TestState {
        count: i32,
    }

    #[derive(Debug)]
    struct IncrementEvent {
        amount: i32,
    }

    #[derive(Debug)]
    struct DecrementEvent {
        amount: i32,
    }

    #[test]
    fn test_reducer_registry_registers() {
        let registry: ReducerRegistry<TestState> = ReducerRegistry::new();

        registry.register(reducer::fold::<IncrementEvent>().into(|state: TestState, event| TestState {
            count: state.count + event.amount,
        }));

        assert_eq!(registry.reducers.read().len(), 1);
    }

    #[test]
    fn test_reducer_registry_applies() {
        let registry: ReducerRegistry<TestState> = ReducerRegistry::new();

        registry.register(reducer::fold::<IncrementEvent>().into(|state: TestState, event| TestState {
            count: state.count + event.amount,
        }));

        let state = TestState { count: 0 };
        let event = IncrementEvent { amount: 5 };

        let new_state = registry.apply(state, TypeId::of::<IncrementEvent>(), &event);
        assert_eq!(new_state.count, 5);
    }

    #[test]
    fn test_reducer_registry_multiple_reducers() {
        let registry: ReducerRegistry<TestState> = ReducerRegistry::new();

        registry.register(reducer::fold::<IncrementEvent>().into(|state: TestState, event| TestState {
            count: state.count + event.amount,
        }));
        registry.register(reducer::fold::<DecrementEvent>().into(|state: TestState, event| TestState {
            count: state.count - event.amount,
        }));

        let state = TestState { count: 10 };

        let inc = IncrementEvent { amount: 5 };
        let state = registry.apply(state, TypeId::of::<IncrementEvent>(), &inc);
        assert_eq!(state.count, 15);

        let dec = DecrementEvent { amount: 3 };
        let state = registry.apply(state, TypeId::of::<DecrementEvent>(), &dec);
        assert_eq!(state.count, 12);
    }

    #[test]
    fn test_reducer_registry_group() {
        let registry: ReducerRegistry<TestState> = ReducerRegistry::new();

        registry.register(reducer::group([
            reducer::fold::<IncrementEvent>().into(|state: TestState, event| TestState {
                count: state.count + event.amount,
            }),
            reducer::fold::<DecrementEvent>().into(|state: TestState, event| TestState {
                count: state.count - event.amount,
            }),
        ]));

        let state = TestState { count: 10 };

        let inc = IncrementEvent { amount: 5 };
        let state = registry.apply(state, TypeId::of::<IncrementEvent>(), &inc);
        assert_eq!(state.count, 15);

        let dec = DecrementEvent { amount: 3 };
        let state = registry.apply(state, TypeId::of::<DecrementEvent>(), &dec);
        assert_eq!(state.count, 12);
    }

    #[test]
    fn test_reducer_registry_unhandled_event() {
        let registry: ReducerRegistry<TestState> = ReducerRegistry::new();

        registry.register(reducer::fold::<IncrementEvent>().into(|state: TestState, event| TestState {
            count: state.count + event.amount,
        }));

        let state = TestState { count: 10 };
        let event = DecrementEvent { amount: 5 };

        // Unhandled event should not change state
        let new_state = registry.apply(state.clone(), TypeId::of::<DecrementEvent>(), &event);
        assert_eq!(new_state.count, state.count);
    }
}
