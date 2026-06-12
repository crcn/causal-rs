//! `ReactorObserver` — telemetry hook for reactor execution and
//! aggregate folds. The engine calls these methods at well-defined
//! points so observability sinks (the in-memory inspector backend,
//! external tracing, etc.) can capture causal-chain history without
//! the engine itself having to know about durability of trace data.
//!
//! All methods have default-noop bodies — implementors override only
//! the events they care about, and engines that don't register an
//! observer pay zero overhead.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::{LogCursor, LogEntry};

/// Sink for reactor and aggregator telemetry. Plumbed into the engine
/// via [`crate::EngineBuilder::with_observer`].
///
/// Methods are called from the engine's hot path. Implementations
/// must be cheap (mutex-and-push, atomic increment, channel send)
/// — heavy work belongs in a downstream consumer.
pub trait ReactorObserver: Send + Sync + 'static {
    /// Called immediately before the runner invokes `react()` for one
    /// trigger event. Captures the start of an attempt.
    fn reactor_started(
        &self,
        _event_id: Uuid,
        _reactor_id: &str,
        _workflow_id: Uuid,
        _attempt: u32,
        _started_at: DateTime<Utc>,
    ) {}

    /// Called after `react()` returned Ok. `logs` are the entries the
    /// reactor body pushed via `ctx.log(...)` during this attempt.
    fn reactor_completed(
        &self,
        _event_id: Uuid,
        _reactor_id: &str,
        _workflow_id: Uuid,
        _attempt: u32,
        _started_at: DateTime<Utc>,
        _completed_at: DateTime<Utc>,
        _logs: &[LogEntry],
    ) {}

    /// Called after `react()` returned Err and the runner is going to
    /// retry. `logs` are the entries pushed before the error.
    fn reactor_failed(
        &self,
        _event_id: Uuid,
        _reactor_id: &str,
        _workflow_id: Uuid,
        _attempt: u32,
        _started_at: DateTime<Utc>,
        _completed_at: DateTime<Utc>,
        _error: &str,
        _logs: &[LogEntry],
    ) {}

    /// Called after `react()` exhausted its retry budget — terminal
    /// failure. The runner advances its cursor past this event and
    /// (if `on_terminal_failure` is configured) synthesizes a downstream fact.
    fn reactor_terminal_failure(
        &self,
        _event_id: Uuid,
        _reactor_id: &str,
        _workflow_id: Uuid,
        _attempts: u32,
        _error: &str,
        _at: DateTime<Utc>,
    ) {}

    /// Called once per aggregator fold (every `AggregatorRegistry::apply_event`).
    /// `state` is the JSON-serialized post-fold aggregate state.
    fn aggregate_folded(
        &self,
        _workflow_id: Uuid,
        _position: LogCursor,
        _event_id: Uuid,
        _aggregate_key: &str,
        _state: serde_json::Value,
    ) {}

    /// Called by the runner BEFORE `react()` if the reactor returned
    /// `Some(_)` from its `describe()` method. Captures the reactor's
    /// declared intent for this trigger event.
    fn reactor_description(
        &self,
        _workflow_id: Uuid,
        _position: LogCursor,
        _event_id: Uuid,
        _reactor_id: &str,
        _description: serde_json::Value,
    ) {}
}

/// Noop observer — every method uses the default impl. Engines with
/// no `.with_observer(...)` call effectively use this.
pub struct NoopObserver;

impl ReactorObserver for NoopObserver {}
