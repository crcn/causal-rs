//! Pluggable runtime for handler execution and state management.
//!
//! Backends provide a runtime that wraps each `effect.call_handler()` call
//! and manages aggregate state. This enables durable execution (Restate),
//! dry-run testing, tracing, etc.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;

use crate::handler::EventOutput;

/// Pluggable runtime for handler execution and state management.
///
/// Backends provide a runtime that wraps each `effect.call_handler()` call
/// and manages aggregate state. The three primitives are:
/// - `run` — wrap a handler invocation (journaling for Restate, pass-through for direct)
/// - `get_state` — read aggregate state by key
/// - `set_state` — write aggregate state by key
///
/// State is type-erased via `Arc<dyn Any + Send + Sync>`. The `DirectRuntime`
/// stores concrete types for zero-cost downcasting. Durable runtimes (Restate,
/// Temporal) serialize at the boundary using the aggregator's closures.
pub trait Runtime: Send + Sync {
    /// Wrap a handler invocation.
    ///
    /// `handler_id` identifies which handler is being called.
    /// `execution` is the handler's future (already constructed but not yet polled).
    fn run(
        &self,
        handler_id: &str,
        execution: Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>>;

    /// Get aggregate state by key. Returns None if no state exists.
    fn get_state(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>>;

    /// Set aggregate state by key.
    fn set_state(&self, key: &str, value: Arc<dyn Any + Send + Sync>);
}

/// Default runtime — executes handlers directly, stores state in-memory.
pub struct DirectRuntime {
    state: DashMap<String, Arc<dyn Any + Send + Sync>>,
}

impl DirectRuntime {
    pub fn new() -> Self {
        Self {
            state: DashMap::new(),
        }
    }
}

impl Default for DirectRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime for DirectRuntime {
    fn run(
        &self,
        _handler_id: &str,
        execution: Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>> {
        execution
    }

    fn get_state(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.state.get(key).map(|v| Arc::clone(v.value()))
    }

    fn set_state(&self, key: &str, value: Arc<dyn Any + Send + Sync>) {
        self.state.insert(key.to_string(), value);
    }
}
