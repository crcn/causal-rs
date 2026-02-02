//! Edge - Clean entry points for event-driven workflows
//!
//! An Edge represents an execution boundary - a point where external inputs
//! enter the system and trigger event flows.
//!
//! # Architecture
//!
//! ```text
//! engine.run(edge, state)
//!     ↓
//! edge.execute() → Event
//!     ↓
//! Reducers: state' = reduce(state, event)
//!     ↓
//! Effects: effect.handle(event, ctx_with_state')
//!     ↓
//! Effects emit new events → Continue until settled
//!     ↓
//! edge.read(final_state) → Result<Data>
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use seesaw_core::{Edge, EdgeContext, Event};
//! use uuid::Uuid;
//!
//! #[derive(Clone)]
//! struct RequestState {
//!     visitor_id: Uuid,
//!     user_id: Option<Uuid>,
//! }
//!
//! #[derive(Clone, Debug)]
//! struct User {
//!     user_id: Uuid,
//!     email: String,
//! }
//!
//! #[derive(Clone)]
//! enum UserEvent {
//!     SignupRequested { email: String, name: String },
//! }
//! // Event is auto-implemented
//!
//! struct SignupEdge {
//!     email: String,
//!     name: String,
//! }
//!
//! impl Edge<RequestState> for SignupEdge {
//!     type Data = User;
//!
//!     fn execute(&self, _ctx: &EdgeContext<RequestState>) -> Option<Box<dyn Event>> {
//!         Some(Box::new(UserEvent::SignupRequested {
//!             email: self.email.clone(),
//!             name: self.name.clone(),
//!         }))
//!     }
//!
//!     fn read(&self, state: &RequestState) -> Option<User> {
//!         state.user_id.map(|user_id| User {
//!             user_id,
//!             email: self.email.clone(),
//!         })
//!     }
//! }
//!
//! // Usage
//! # async fn example() -> anyhow::Result<()> {
//! # let email = "test@example.com".to_string();
//! # let name = "Test User".to_string();
//! # let engine: seesaw_core::Engine<()> = todo!();
//! let state = RequestState {
//!     visitor_id: Uuid::new_v4(),
//!     user_id: None,
//! };
//!
//! let user = engine.run(SignupEdge { email, name }, state).await?
//!     .ok_or_else(|| anyhow::anyhow!("signup failed"))?;
//! # Ok(())
//! # }
//! ```

use crate::core::Event;
use std::sync::Arc;

/// Edge trait - Clean entry point for event-driven workflows
///
/// An Edge represents an execution boundary where external inputs
/// enter the system. It:
/// 1. Emits an initial event via `execute()`
/// 2. Reads final result from state via `read()`
///
/// Edges are pure and stateless. All state flows through the generic `S` parameter.
pub trait Edge<S>: Send + Sync + 'static {
    /// The event type this edge emits
    type Event: Event;

    /// The data type returned after execution completes
    type Data;

    /// Execute the edge and return an initial event
    ///
    /// This is called once at the start of execution. Return:
    /// - `Some(event)` to trigger event flow
    /// - `None` for query-only edges (no event emission)
    fn execute(&self, ctx: &EdgeContext<S>) -> Option<Self::Event>;

    /// Read the final result from the settled state
    ///
    /// Called after all events have settled. Return:
    /// - `Some(data)` if the workflow succeeded
    /// - `None` if the workflow failed or data is unavailable
    fn read(&self, state: &S) -> Option<Self::Data>;
}

/// Context provided to edges during execution
///
/// Currently minimal - may be extended with additional context
/// like correlation IDs, tracing, etc.
pub struct EdgeContext<S> {
    _state: Arc<S>,
}

impl<S> EdgeContext<S> {
    /// Create a new EdgeContext
    pub fn new(state: Arc<S>) -> Self {
        Self { _state: state }
    }

    /// Get the current state
    pub fn state(&self) -> &S {
        &self._state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct TestEvent {
        value: i32,
    }

    // Event is auto-implemented via blanket impl

    #[derive(Clone)]
    struct TestState {
        result: Option<i32>,
    }

    struct TestEdge {
        initial_value: i32,
    }

    impl Edge<TestState> for TestEdge {
        type Event = TestEvent;
        type Data = i32;

        fn execute(&self, _ctx: &EdgeContext<TestState>) -> Option<TestEvent> {
            Some(TestEvent {
                value: self.initial_value,
            })
        }

        fn read(&self, state: &TestState) -> Option<i32> {
            state.result
        }
    }

    #[test]
    fn test_edge_execute() {
        let state = TestState { result: None };
        let edge = TestEdge { initial_value: 42 };
        let ctx = EdgeContext::new(Arc::new(state));

        let event = edge.execute(&ctx).unwrap();
        assert_eq!(event.value, 42);
    }

    #[test]
    fn test_edge_read() {
        let state = TestState { result: Some(100) };
        let edge = TestEdge { initial_value: 42 };

        let result = edge.read(&state);
        assert_eq!(result, Some(100));
    }

    #[test]
    fn test_edge_read_none() {
        let state = TestState { result: None };
        let edge = TestEdge { initial_value: 42 };

        let result = edge.read(&state);
        assert_eq!(result, None);
    }
}
