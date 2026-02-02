//! Reducer builder functions and types.

use std::any::TypeId;
use std::marker::PhantomData;
use std::sync::Arc;

use super::types::{AnyEventRef, Reducer};

// =============================================================================
// on::<E>() - Single typed event
// =============================================================================

/// Create a reducer that handles a specific event type.
///
/// # Example
///
/// ```ignore
/// reducer::on::<Increment>().run(|state, event| {
///     State { count: state.count + event.amount, ..state }
/// })
/// ```
pub fn on<E: Send + Sync + 'static>() -> OnBuilder<E> {
    OnBuilder {
        _marker: PhantomData,
    }
}

/// Builder for typed event reducers.
pub struct OnBuilder<E> {
    _marker: PhantomData<E>,
}

impl<E: Send + Sync + 'static> OnBuilder<E> {
    /// Set the reducer function (terminal operation).
    pub fn run<S, R>(self, reducer: R) -> Reducer<S>
    where
        S: Clone + Send + Sync + 'static,
        R: Fn(S, &E) -> S + Send + Sync + 'static,
    {
        let target = TypeId::of::<E>();
        Reducer {
            can_handle: Arc::new(move |t| t == target),
            reduce: Arc::new(move |state, event, _| {
                let typed = event.downcast_ref::<E>().expect("type checked by can_handle");
                reducer(state, typed)
            }),
        }
    }
}

// =============================================================================
// on_any() - All events
// =============================================================================

/// Create a reducer that handles all events (type-erased).
///
/// # Example
///
/// ```ignore
/// reducer::on_any().run(|state, event| {
///     if let Some(inc) = event.downcast_ref::<Increment>() {
///         return State { count: state.count + inc.amount, ..state };
///     }
///     state
/// })
/// ```
pub fn on_any() -> OnAnyBuilder {
    OnAnyBuilder {}
}

/// Builder for reducers that handle all events.
pub struct OnAnyBuilder {}

impl OnAnyBuilder {
    /// Set the reducer function (terminal operation).
    pub fn run<S, R>(self, reducer: R) -> Reducer<S>
    where
        S: Clone + Send + Sync + 'static,
        R: Fn(S, AnyEventRef) -> S + Send + Sync + 'static,
    {
        Reducer {
            can_handle: Arc::new(|_| true),
            reduce: Arc::new(move |state, event, type_id| {
                let event_ref = AnyEventRef {
                    value: event,
                    type_id,
                };
                reducer(state, event_ref)
            }),
        }
    }
}

// =============================================================================
// group() - Compose multiple reducers
// =============================================================================

/// Compose multiple reducers into a single reducer.
///
/// Reducers are applied in order. Each reducer receives the state
/// produced by the previous reducer.
///
/// # Example
///
/// ```ignore
/// reducer::group([
///     reducer::on::<EventA>().run(reduce_a),
///     reducer::on::<EventB>().run(reduce_b),
///     reducer::on::<EventC>().run(reduce_c),
/// ])
/// ```
pub fn group<S>(reducers: impl IntoIterator<Item = Reducer<S>>) -> Reducer<S>
where
    S: Clone + Send + Sync + 'static,
{
    let reducers: Arc<Vec<Reducer<S>>> = Arc::new(reducers.into_iter().collect());

    Reducer {
        can_handle: {
            let reducers = reducers.clone();
            Arc::new(move |type_id| reducers.iter().any(|r| (r.can_handle)(type_id)))
        },
        reduce: {
            let reducers = reducers.clone();
            Arc::new(move |state, event, type_id| {
                let mut state = state;
                for reducer in reducers.iter() {
                    if (reducer.can_handle)(type_id) {
                        state = (reducer.reduce)(state, event, type_id);
                    }
                }
                state
            })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct TestState {
        count: i32,
    }

    struct Increment {
        amount: i32,
    }

    struct Decrement {
        amount: i32,
    }

    struct OtherEvent;

    #[test]
    fn test_on_can_handle() {
        let reducer: Reducer<TestState> = on::<Increment>().run(|state: TestState, event| TestState {
            count: state.count + event.amount,
        });

        assert!(reducer.can_handle(TypeId::of::<Increment>()));
        assert!(!reducer.can_handle(TypeId::of::<Decrement>()));
        assert!(!reducer.can_handle(TypeId::of::<OtherEvent>()));
    }

    #[test]
    fn test_on_any_can_handle() {
        let reducer: Reducer<TestState> = on_any().run(|state: TestState, _| state);

        assert!(reducer.can_handle(TypeId::of::<Increment>()));
        assert!(reducer.can_handle(TypeId::of::<Decrement>()));
        assert!(reducer.can_handle(TypeId::of::<OtherEvent>()));
    }

    #[test]
    fn test_on_reduces() {
        let reducer: Reducer<TestState> = on::<Increment>().run(|state: TestState, event| TestState {
            count: state.count + event.amount,
        });

        let state = TestState { count: 10 };
        let event = Increment { amount: 5 };

        let new_state = reducer.apply(state, &event, TypeId::of::<Increment>());
        assert_eq!(new_state.count, 15);
    }

    #[test]
    fn test_on_any_reduces() {
        let reducer: Reducer<TestState> = on_any().run(|state: TestState, event| {
            if let Some(inc) = event.downcast_ref::<Increment>() {
                return TestState {
                    count: state.count + inc.amount,
                };
            }
            if let Some(dec) = event.downcast_ref::<Decrement>() {
                return TestState {
                    count: state.count - dec.amount,
                };
            }
            state
        });

        let state = TestState { count: 10 };

        let inc = Increment { amount: 5 };
        let state = reducer.apply(state, &inc, TypeId::of::<Increment>());
        assert_eq!(state.count, 15);

        let dec = Decrement { amount: 3 };
        let state = reducer.apply(state, &dec, TypeId::of::<Decrement>());
        assert_eq!(state.count, 12);
    }

    #[test]
    fn test_group_can_handle() {
        let reducer: Reducer<TestState> = group([
            on::<Increment>().run(|state: TestState, event| TestState {
                count: state.count + event.amount,
            }),
            on::<Decrement>().run(|state: TestState, event| TestState {
                count: state.count - event.amount,
            }),
        ]);

        assert!(reducer.can_handle(TypeId::of::<Increment>()));
        assert!(reducer.can_handle(TypeId::of::<Decrement>()));
        assert!(!reducer.can_handle(TypeId::of::<OtherEvent>()));
    }

    #[test]
    fn test_group_reduces() {
        let reducer: Reducer<TestState> = group([
            on::<Increment>().run(|state: TestState, event| TestState {
                count: state.count + event.amount,
            }),
            on::<Decrement>().run(|state: TestState, event| TestState {
                count: state.count - event.amount,
            }),
        ]);

        let state = TestState { count: 10 };

        let inc = Increment { amount: 5 };
        let state = reducer.apply(state, &inc, TypeId::of::<Increment>());
        assert_eq!(state.count, 15);

        let dec = Decrement { amount: 3 };
        let state = reducer.apply(state, &dec, TypeId::of::<Decrement>());
        assert_eq!(state.count, 12);
    }

    #[test]
    fn test_reducer_clone() {
        let reducer: Reducer<TestState> = on::<Increment>().run(|state: TestState, event| TestState {
            count: state.count + event.amount,
        });

        let reducer2 = reducer.clone();

        let state = TestState { count: 10 };
        let event = Increment { amount: 5 };

        let new_state1 = reducer.apply(state.clone(), &event, TypeId::of::<Increment>());
        let new_state2 = reducer2.apply(state, &event, TypeId::of::<Increment>());

        assert_eq!(new_state1.count, 15);
        assert_eq!(new_state2.count, 15);
    }
}
