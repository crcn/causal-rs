//! Error types for service layer.

use thiserror::Error;

/// Errors that can occur during service operations.
#[derive(Error, Debug)]
pub enum ServiceError {
    /// A spawned task failed during execution.
    #[error("Task failed: {0}")]
    TaskFailed(#[source] anyhow::Error),

    /// An error occurred while handling an effect.
    #[error("Effect error for event '{event_type}' in effect '{effect_type}': {source}")]
    EffectError {
        /// The type name of the event that was being handled.
        event_type: &'static str,
        /// The type name of the effect that failed.
        effect_type: &'static str,
        /// The underlying error.
        #[source]
        source: anyhow::Error,
    },

    /// Multiple errors occurred while settling pending tasks.
    #[error("Settling failed: {0}")]
    SettleError(#[source] anyhow::Error),

    /// The action result failed to read.
    #[error("Action result read failed: {0}")]
    ActionReadError(#[source] anyhow::Error),
}

impl ServiceError {
    /// Create a new effect error.
    pub fn effect_error<E: Into<anyhow::Error>>(
        event_type: &'static str,
        effect_type: &'static str,
        source: E,
    ) -> Self {
        Self::EffectError {
            event_type,
            effect_type,
            source: source.into(),
        }
    }

    /// Create a new task failed error.
    pub fn task_failed<E: Into<anyhow::Error>>(source: E) -> Self {
        Self::TaskFailed(source.into())
    }

    /// Create a new settle error.
    pub fn settle_error<E: Into<anyhow::Error>>(source: E) -> Self {
        Self::SettleError(source.into())
    }
}

/// Result type alias using `ServiceError`.
pub type Result<T> = std::result::Result<T, ServiceError>;
