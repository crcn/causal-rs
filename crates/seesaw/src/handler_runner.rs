//! Pluggable execution wrapper for handler invocations.
//!
//! Backends provide a runner that wraps each `effect.call_handler()` call.
//! This enables durable execution (Restate), dry-run testing, tracing, etc.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::handler::EventOutput;

/// Pluggable execution wrapper for handler invocations.
///
/// Backends provide a runner that wraps each `effect.call_handler()` call.
/// This enables durable execution (Restate), dry-run testing, tracing, etc.
pub trait HandlerRunner: Send + Sync {
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

/// Default runner — executes handlers directly. Current behavior.
pub struct DirectRunner;

impl HandlerRunner for DirectRunner {
    fn run(
        &self,
        _handler_id: &str,
        execution: Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>> {
        execution
    }
}
