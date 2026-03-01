//! Pluggable runtime for handler execution.
//!
//! Backends provide a runtime that wraps each `effect.call_handler()` call.
//! This enables durable execution (Restate), dry-run testing, tracing, etc.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::handler::EventOutput;

/// Pluggable runtime for handler execution.
///
/// Backends provide a runtime that wraps each `effect.call_handler()` call.
/// The single primitive is:
/// - `run` — wrap a handler invocation (journaling for Restate, pass-through for direct)
///
/// Aggregate state is managed internally by `AggregatorRegistry` (no runtime involvement).
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
}

/// Default runtime — executes handlers directly.
pub struct DirectRuntime;

impl DirectRuntime {
    pub fn new() -> Self {
        Self
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
}
