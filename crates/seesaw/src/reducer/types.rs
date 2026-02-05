//! Core reducer types.

use std::any::{Any, TypeId};
use std::sync::Arc;

use crate::event_codec::EventCodec;

/// Reference to a type-erased event for reducers that handle all events.
pub struct AnyEventRef<'a> {
    /// Reference to the type-erased event value.
    pub value: &'a dyn Any,
    /// The TypeId of the original event.
    pub type_id: TypeId,
}

impl<'a> AnyEventRef<'a> {
    /// Attempt to downcast to a specific event type.
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.value.downcast_ref::<T>()
    }

    /// Check if this event is of a specific type.
    pub fn is<T: 'static>(&self) -> bool {
        self.type_id == TypeId::of::<T>()
    }
}

/// A reducer - no traits, just data with closures.
///
/// Reducers are pure functions that take the current state and an event,
/// returning a new state. They are executed serially under a write lock.
pub struct Reducer<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Queue codec metadata for typed event reducers.
    pub(crate) codecs: Vec<Arc<EventCodec>>,

    /// Determines if this reducer handles the given event type.
    pub(crate) can_handle: Arc<dyn Fn(TypeId) -> bool + Send + Sync>,

    /// Applies the reducer to produce a new state.
    /// Receives the current state, the type-erased event, and its TypeId.
    pub(crate) reduce: Arc<dyn Fn(S, &dyn Any, TypeId) -> S + Send + Sync>,
}

impl<S> Clone for Reducer<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            codecs: self.codecs.clone(),
            can_handle: self.can_handle.clone(),
            reduce: self.reduce.clone(),
        }
    }
}

impl<S> Reducer<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Check if this reducer handles the given event type.
    pub fn can_handle(&self, type_id: TypeId) -> bool {
        (self.can_handle)(type_id)
    }

    /// Apply the reducer to produce a new state.
    pub fn apply(&self, state: S, event: &dyn Any, type_id: TypeId) -> S {
        (self.reduce)(state, event, type_id)
    }

    /// Internal queue codec metadata, if available.
    pub(crate) fn codecs(&self) -> &[Arc<EventCodec>] {
        &self.codecs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_any_event_ref_downcast() {
        let value = 42i32;
        let event = AnyEventRef {
            value: &value,
            type_id: TypeId::of::<i32>(),
        };

        assert!(event.is::<i32>());
        assert!(!event.is::<String>());

        let downcasted = event.downcast_ref::<i32>();
        assert!(downcasted.is_some());
        assert_eq!(*downcasted.unwrap(), 42);

        let wrong_type = event.downcast_ref::<String>();
        assert!(wrong_type.is_none());
    }
}
