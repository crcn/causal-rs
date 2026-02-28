//! Pluggable runtime for handler execution and state management.
//!
//! Backends provide a runtime that wraps each `effect.call_handler()` call
//! and manages aggregate state. This enables durable execution (Restate),
//! dry-run testing, tracing, etc.

use std::future::Future;
use std::pin::Pin;

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
    fn get_state(&self, key: &str) -> Option<serde_json::Value>;

    /// Set aggregate state by key.
    fn set_state(&self, key: &str, value: serde_json::Value);
}

/// Default runtime — executes handlers directly, stores state in-memory.
pub struct DirectRuntime {
    state: DashMap<String, serde_json::Value>,
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

    fn get_state(&self, key: &str) -> Option<serde_json::Value> {
        self.state.get(key).map(|v| v.value().clone())
    }

    fn set_state(&self, key: &str, value: serde_json::Value) {
        self.state.insert(key.to_string(), value);
    }
}
