//! Structured error types, batch outcomes, and failure events for seesaw.
//!
//! `SeesawError` provides pattern-matchable errors instead of generic `anyhow::Error`.
//! `BatchOutcome` reports what actually happened during batch execution.
//! `CommandFailed` is a domain event emitted when effects return errors.
//!
//! # The Error Boundary Rule
//!
//! > **No `anyhow::Error` ever crosses the EventBus boundary.**
//!
//! - `anyhow` is internal transport (ergonomic for effects)
//! - `CommandFailed` is the only externalized error (structured for domain)
//!
//! # CommandFailed Example
//!
//! ```ignore
//! use seesaw::{Machine, CommandFailed, SafeErrorCategory};
//!
//! // Agents/machines can observe and react to failures
//! impl Machine for RecoveryMachine {
//!     type Event = CommandFailed;
//!     type Command = RecoveryCommand;
//!
//!     fn decide(&mut self, event: &CommandFailed) -> Option<RecoveryCommand> {
//!         match event.category {
//!             SafeErrorCategory::Validation => Some(RecoveryCommand::RetryWithCorrection),
//!             SafeErrorCategory::RateLimited => Some(RecoveryCommand::BackoffAndRetry),
//!             _ => None,
//!         }
//!     }
//! }
//! ```
//!
//! # Error Example
//!
//! ```ignore
//! use seesaw::{Dispatcher, SeesawError};
//!
//! let result = dispatcher.dispatch(vec![cmd]).await;
//! match result {
//!     Ok(()) => println!("Success!"),
//!     Err(e) => {
//!         if let Some(seesaw_err) = e.downcast_ref::<SeesawError>() {
//!             match seesaw_err {
//!                 SeesawError::NoEffectRegistered { type_name } => {
//!                     eprintln!("No effect for command type: {}", type_name);
//!                 }
//!                 SeesawError::EffectAlreadyRegistered { type_name } => {
//!                     eprintln!("Effect already exists for: {}", type_name);
//!                 }
//!                 _ => eprintln!("Other seesaw error: {}", seesaw_err),
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! # BatchOutcome Example
//!
//! ```ignore
//! use seesaw::{BatchOutcome, Effect, EffectContext};
//!
//! async fn execute_batch(&self, cmds: Vec<MyCmd>, ctx: EffectContext<D>) -> Result<BatchOutcome> {
//!     for (i, cmd) in cmds.into_iter().enumerate() {
//!         if let Err(e) = self.execute(cmd, ctx.clone()).await {
//!             return Ok(BatchOutcome::Partial {
//!                 succeeded: i,
//!                 failed_at: i,
//!                 error: e,
//!             });
//!         }
//!     }
//!     Ok(BatchOutcome::Complete)
//! }
//! ```

use std::any::TypeId;
use std::borrow::Cow;
use std::fmt;

use thiserror::Error;

use crate::core::CorrelationId;

// =============================================================================
// Command Failed Event
// =============================================================================

/// Error category for sanitized failure events.
///
/// This enum categorizes errors for safe external exposure.
/// Internal error details are NEVER exposed - only the category.
///
/// # Security Rules
///
/// - `Validation`: Safe to expose details (user input errors)
/// - `NotFound`: Safe to expose (resource not found)
/// - `Unauthorized`: NEVER expose details (auth failure)
/// - `RateLimited`: Safe to expose (rate limit hit)
/// - `InternalError`: NEVER expose details (server error)
/// - `ExternalService`: NEVER expose details (third-party failure)
/// - `AIFailure`: Expose only structured data (no prompts/responses)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeErrorCategory {
    /// User input validation errors - safe to expose details.
    Validation,
    /// Resource not found - safe to expose.
    NotFound,
    /// Authentication/authorization failure - NEVER expose details.
    Unauthorized,
    /// Rate limit exceeded - safe to expose.
    RateLimited,
    /// Internal server error - NEVER expose details.
    InternalError,
    /// External service failure - NEVER expose details.
    ExternalService,
    /// AI-specific failure - expose only structured retry info.
    AIFailure,
}

impl fmt::Display for SafeErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SafeErrorCategory::Validation => write!(f, "validation_error"),
            SafeErrorCategory::NotFound => write!(f, "not_found"),
            SafeErrorCategory::Unauthorized => write!(f, "unauthorized"),
            SafeErrorCategory::RateLimited => write!(f, "rate_limited"),
            SafeErrorCategory::InternalError => write!(f, "internal_error"),
            SafeErrorCategory::ExternalService => write!(f, "external_service_error"),
            SafeErrorCategory::AIFailure => write!(f, "ai_failure"),
        }
    }
}

// =============================================================================
// Categorizable Trait
// =============================================================================

/// Trait for errors that can be categorized for safe external exposure.
///
/// Implement this trait on domain error types to enable automatic categorization
/// in `CommandFailed::from_error()`.
///
/// # Safe Message Contract
///
/// - `Validation` and `NotFound` categories MAY return their Display string verbatim
/// - `Unauthorized`, `InternalError`, `ExternalService` MUST return generic messages
/// - Static strings are preferred; owned strings are allowed when needed
///
/// # Example
///
/// ```ignore
/// use seesaw::{Categorizable, SafeErrorCategory};
/// use std::borrow::Cow;
///
/// impl Categorizable for MyDomainError {
///     fn category(&self) -> SafeErrorCategory {
///         match self {
///             MyDomainError::NotFound(_) => SafeErrorCategory::NotFound,
///             MyDomainError::InvalidInput(_) => SafeErrorCategory::Validation,
///             MyDomainError::AccessDenied => SafeErrorCategory::Unauthorized,
///             _ => SafeErrorCategory::InternalError,
///         }
///     }
///
///     fn safe_message(&self) -> Cow<'static, str> {
///         match self {
///             // NotFound - safe to expose details
///             MyDomainError::NotFound(id) => format!("Resource not found: {}", id).into(),
///             // Validation - safe to expose details
///             MyDomainError::InvalidInput(msg) => format!("Invalid input: {}", msg).into(),
///             // Unauthorized - generic message only
///             MyDomainError::AccessDenied => "Access denied".into(),
///             // InternalError - generic message only
///             _ => "An internal error occurred".into(),
///         }
///     }
/// }
/// ```
pub trait Categorizable: std::error::Error {
    /// Return the safe category for this error.
    fn category(&self) -> SafeErrorCategory;

    /// Return a sanitized, user-safe message.
    ///
    /// Only `Validation` and `NotFound` errors may expose specific details.
    /// All other categories must return generic messages.
    fn safe_message(&self) -> Cow<'static, str>;
}

/// A domain event emitted when an effect returns an error.
///
/// This is the ONLY error type that crosses the EventBus boundary.
/// All internal `anyhow::Error` details are sanitized before emission.
///
/// # Key Properties
///
/// - **Machine-observable**: Agents and machines can match on this event
/// - **Safe for clients**: No internal details, stack traces, or PII
/// - **Correlated**: Carries the original command's correlation ID
///
/// # Example
///
/// ```ignore
/// // The dispatcher automatically emits CommandFailed on error:
/// match effect.execute(cmd, ctx).await {
///     Ok(event) => bus.emit(event),
///     Err(e) => bus.emit(CommandFailed::from_error(&e, "CreateUser", cid)),
/// }
///
/// // Agents can react to failures:
/// impl Machine for AgentRecoveryMachine {
///     type Event = CommandFailed;
///     type Command = RetryCommand;
///
///     fn decide(&mut self, event: &CommandFailed) -> Option<RetryCommand> {
///         match event.category {
///             SafeErrorCategory::Validation => {
///                 // Agent can self-correct
///                 Some(RetryCommand::WithCorrection { hint: event.safe_message.clone() })
///             }
///             SafeErrorCategory::RateLimited => {
///                 Some(RetryCommand::Backoff { seconds: 30 })
///             }
///             _ => None, // Can't self-correct
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct CommandFailed {
    /// The type name of the command that failed (externalized, not internal).
    pub command_type: &'static str,
    /// The category of failure (safe for external consumption).
    pub category: SafeErrorCategory,
    /// A sanitized, user-safe error message.
    ///
    /// This message is safe to display to users. It contains NO:
    /// - Stack traces
    /// - Internal error details
    /// - Database column names
    /// - PII
    pub safe_message: String,
    /// The correlation ID of the original command.
    pub cid: CorrelationId,
}

impl CommandFailed {
    /// Create a CommandFailed event from an anyhow error.
    ///
    /// This method sanitizes the error before creating the event.
    /// The raw error should be logged separately for debugging.
    ///
    /// # Arguments
    ///
    /// * `error` - The internal error (will be sanitized)
    /// * `command_type` - The command type name (externalized)
    /// * `cid` - The correlation ID for tracking
    ///
    /// # Example
    ///
    /// ```ignore
    /// // In dispatcher
    /// Err(e) => {
    ///     // Log raw error for developers (before sanitization)
    ///     tracing::error!(%cid, error = ?e, "effect failed");
    ///
    ///     // Emit sanitized event
    ///     bus.emit(CommandFailed::from_error(&e, "CreateUser", cid));
    /// }
    /// ```
    pub fn from_error(
        error: &anyhow::Error,
        command_type: &'static str,
        cid: CorrelationId,
    ) -> Self {
        let (category, safe_message) = Self::categorize_and_sanitize(error);
        Self {
            command_type,
            category,
            safe_message,
            cid,
        }
    }

    /// Categorize and sanitize an error for external consumption.
    ///
    /// Uses anyhow's downcasting to identify specific error types that implement
    /// [`Categorizable`]. Falls back to InternalError with a generic message.
    ///
    /// # Error Type Priority
    ///
    /// Errors are checked in order of specificity. Add new domain error types
    /// to this function as they implement `Categorizable`.
    ///
    /// # Note
    ///
    /// Rust cannot downcast to `dyn Categorizable` - we must use concrete types.
    /// If this list grows large, consider generating it with a macro.
    fn categorize_and_sanitize(error: &anyhow::Error) -> (SafeErrorCategory, String) {
        // =================================================================
        // Seesaw framework errors (Categorizable)
        // =================================================================
        if let Some(e) = error.downcast_ref::<SeesawError>() {
            return (e.category(), e.safe_message().into_owned());
        }

        // =================================================================
        // Domain errors (add new Categorizable types here)
        // =================================================================
        // TODO: Add domain errors as they implement Categorizable:
        // if let Some(e) = error.downcast_ref::<DeckError>() {
        //     return (e.category(), e.safe_message().into_owned());
        // }
        // if let Some(e) = error.downcast_ref::<AuthError>() {
        //     return (e.category(), e.safe_message().into_owned());
        // }

        // =================================================================
        // Standard library errors
        // =================================================================
        if let Some(io_err) = error.downcast_ref::<std::io::Error>() {
            return match io_err.kind() {
                std::io::ErrorKind::NotFound => {
                    (SafeErrorCategory::NotFound, "Resource not found".into())
                }
                std::io::ErrorKind::PermissionDenied => {
                    (SafeErrorCategory::Unauthorized, "Access denied".into())
                }
                _ => (
                    SafeErrorCategory::InternalError,
                    "An internal error occurred".into(),
                ),
            };
        }

        // =================================================================
        // Default fallback
        // =================================================================
        // NEVER use error.to_string() here - it may contain sensitive data
        (
            SafeErrorCategory::InternalError,
            "An internal error occurred".into(),
        )
    }

    /// Create a validation error with a safe message.
    pub fn validation(
        command_type: &'static str,
        message: impl Into<String>,
        cid: CorrelationId,
    ) -> Self {
        Self {
            command_type,
            category: SafeErrorCategory::Validation,
            safe_message: message.into(),
            cid,
        }
    }

    /// Create a not-found error.
    pub fn not_found(
        command_type: &'static str,
        resource: impl Into<String>,
        cid: CorrelationId,
    ) -> Self {
        Self {
            command_type,
            category: SafeErrorCategory::NotFound,
            safe_message: format!("{} not found", resource.into()),
            cid,
        }
    }

    /// Create an unauthorized error (generic message, no details).
    pub fn unauthorized(command_type: &'static str, cid: CorrelationId) -> Self {
        Self {
            command_type,
            category: SafeErrorCategory::Unauthorized,
            safe_message: "Access denied".into(),
            cid,
        }
    }

    /// Create a rate-limited error.
    pub fn rate_limited(command_type: &'static str, cid: CorrelationId) -> Self {
        Self {
            command_type,
            category: SafeErrorCategory::RateLimited,
            safe_message: "Rate limit exceeded. Please try again later.".into(),
            cid,
        }
    }
}

impl fmt::Display for CommandFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "command {} failed ({}): {}",
            self.command_type, self.category, self.safe_message
        )
    }
}

// CommandFailed automatically implements Event via blanket impl
// (Clone + Send + Sync + 'static)

// =============================================================================
// Batch Outcome
// =============================================================================

/// Outcome of a batch effect execution.
///
/// Seesaw does not pretend batches are atomic. Instead, it reports exactly
/// what happened so the caller can:
/// - Track progress accurately
/// - Resume from failures safely
/// - Make informed retry decisions
///
/// # Key Principle
///
/// Seesaw exposes truth, not timing. Batches report what actually happened.
#[allow(dead_code)]
#[derive(Debug)]
pub enum BatchOutcome {
    /// All commands in the batch executed successfully.
    Complete,

    /// Some commands succeeded before a failure occurred.
    ///
    /// The `succeeded` count reflects commands that ran without error.
    /// The `failed_at` index identifies which command failed.
    /// Commands after `failed_at` were not executed.
    Partial {
        /// Number of commands that succeeded (0..failed_at).
        succeeded: usize,
        /// Index of the command that failed.
        failed_at: usize,
        /// The error that caused the failure.
        error: anyhow::Error,
    },
}

#[allow(dead_code)]
impl BatchOutcome {
    /// Returns true if all commands completed successfully.
    pub fn is_complete(&self) -> bool {
        matches!(self, BatchOutcome::Complete)
    }

    /// Returns true if the batch failed partway through.
    pub fn is_partial(&self) -> bool {
        matches!(self, BatchOutcome::Partial { .. })
    }

    /// Returns the number of commands that succeeded.
    pub fn succeeded_count(&self, total: usize) -> usize {
        match self {
            BatchOutcome::Complete => total,
            BatchOutcome::Partial { succeeded, .. } => *succeeded,
        }
    }
}

impl fmt::Display for BatchOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchOutcome::Complete => write!(f, "batch complete"),
            BatchOutcome::Partial {
                succeeded,
                failed_at,
                error,
            } => {
                write!(
                    f,
                    "batch partial: {} succeeded, failed at index {}: {}",
                    succeeded, failed_at, error
                )
            }
        }
    }
}

// =============================================================================
// Seesaw Error
// =============================================================================

/// Structured error type for seesaw operations.
///
/// This enum provides pattern-matchable errors for common failure modes.
/// Each variant includes context about what went wrong.
#[derive(Debug, Error)]
pub enum SeesawError {
    /// No effect handler is registered for the given command type.
    #[error("no effect registered for command type {type_name}")]
    NoEffectRegistered {
        /// The TypeId of the command that has no effect.
        type_id: TypeId,
        /// Human-readable type name.
        type_name: &'static str,
    },

    /// An effect is already registered for this command type.
    #[error("effect already registered for command type {type_name}")]
    EffectAlreadyRegistered {
        /// Human-readable type name of the command.
        type_name: &'static str,
    },

    /// Command type mismatch during dispatch (internal error).
    #[error("command type mismatch: expected {expected}")]
    CommandTypeMismatch {
        /// Expected type name.
        expected: &'static str,
        /// Actual TypeId received.
        actual_type_id: TypeId,
    },

    /// Timeout waiting for correlated work to complete.
    #[error("operation timed out after {duration:?}")]
    Timeout {
        /// How long we waited.
        duration: std::time::Duration,
    },

    /// Background command enqueue failed.
    #[error("failed to enqueue background command: {message}")]
    BackgroundEnqueueFailed {
        /// The underlying error message.
        message: String,
    },

    /// Scheduled command scheduling failed.
    #[error("failed to schedule command: {message}")]
    ScheduleFailed {
        /// The underlying error message.
        message: String,
    },
}

impl Categorizable for SeesawError {
    fn category(&self) -> SafeErrorCategory {
        // All SeesawError variants are internal errors
        SafeErrorCategory::InternalError
    }

    fn safe_message(&self) -> Cow<'static, str> {
        // InternalError category - return generic messages only
        match self {
            SeesawError::Timeout { .. } => "Operation timed out".into(),
            _ => "An internal error occurred".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_effect_registered_display() {
        let err = SeesawError::NoEffectRegistered {
            type_id: TypeId::of::<String>(),
            type_name: "MyCommand",
        };
        assert!(err.to_string().contains("no effect registered"));
        assert!(err.to_string().contains("MyCommand"));
    }

    #[test]
    fn test_effect_already_registered_display() {
        let err = SeesawError::EffectAlreadyRegistered {
            type_name: "MyCommand",
        };
        assert!(err.to_string().contains("already registered"));
        assert!(err.to_string().contains("MyCommand"));
    }

    #[test]
    fn test_timeout_display() {
        let err = SeesawError::Timeout {
            duration: std::time::Duration::from_secs(30),
        };
        assert!(err.to_string().contains("timed out"));
        assert!(err.to_string().contains("30"));
    }

    #[test]
    fn test_error_is_pattern_matchable() {
        let err = SeesawError::NoEffectRegistered {
            type_id: TypeId::of::<String>(),
            type_name: "TestCommand",
        };

        // This is the key benefit - we can pattern match!
        match &err {
            SeesawError::NoEffectRegistered { type_name, .. } => {
                assert_eq!(*type_name, "TestCommand");
            }
            _ => panic!("Expected NoEffectRegistered"),
        }
    }

    #[test]
    fn test_error_can_be_downcast_from_anyhow() {
        let err: anyhow::Error = SeesawError::NoEffectRegistered {
            type_id: TypeId::of::<String>(),
            type_name: "TestCommand",
        }
        .into();

        // Can downcast from anyhow::Error
        let seesaw_err = err.downcast_ref::<SeesawError>();
        assert!(seesaw_err.is_some());

        match seesaw_err.unwrap() {
            SeesawError::NoEffectRegistered { type_name, .. } => {
                assert_eq!(*type_name, "TestCommand");
            }
            _ => panic!("Expected NoEffectRegistered"),
        }
    }

    // ==========================================================================
    // BatchOutcome Tests
    // ==========================================================================

    #[test]
    fn test_batch_outcome_complete() {
        let outcome = BatchOutcome::Complete;
        assert!(outcome.is_complete());
        assert!(!outcome.is_partial());
        assert_eq!(outcome.succeeded_count(5), 5);
        assert!(outcome.to_string().contains("complete"));
    }

    #[test]
    fn test_batch_outcome_partial() {
        let outcome = BatchOutcome::Partial {
            succeeded: 2,
            failed_at: 2,
            error: anyhow::anyhow!("command 2 failed"),
        };
        assert!(!outcome.is_complete());
        assert!(outcome.is_partial());
        assert_eq!(outcome.succeeded_count(5), 2);

        let display = outcome.to_string();
        assert!(display.contains("partial"));
        assert!(display.contains("2 succeeded"));
        assert!(display.contains("failed at index 2"));
    }

    #[test]
    fn test_batch_outcome_partial_first_command_fails() {
        let outcome = BatchOutcome::Partial {
            succeeded: 0,
            failed_at: 0,
            error: anyhow::anyhow!("first command failed"),
        };
        assert!(outcome.is_partial());
        assert_eq!(outcome.succeeded_count(5), 0);
    }

    // ==========================================================================
    // Categorizable Trait Tests
    // ==========================================================================

    #[test]
    fn test_seesaw_error_category_is_internal() {
        let errors = vec![
            SeesawError::NoEffectRegistered {
                type_id: TypeId::of::<String>(),
                type_name: "TestCommand",
            },
            SeesawError::EffectAlreadyRegistered {
                type_name: "TestCommand",
            },
            SeesawError::Timeout {
                duration: std::time::Duration::from_secs(30),
            },
        ];

        for err in errors {
            assert_eq!(err.category(), SafeErrorCategory::InternalError);
        }
    }

    #[test]
    fn test_seesaw_error_safe_message_is_generic() {
        // Internal errors should return generic messages
        let err = SeesawError::NoEffectRegistered {
            type_id: TypeId::of::<String>(),
            type_name: "SensitiveCommandName",
        };
        let safe_msg = err.safe_message();
        // Should NOT contain the command name (could be sensitive)
        assert!(!safe_msg.contains("SensitiveCommandName"));
        assert_eq!(safe_msg, "An internal error occurred");
    }

    #[test]
    fn test_timeout_safe_message() {
        let err = SeesawError::Timeout {
            duration: std::time::Duration::from_secs(30),
        };
        let safe_msg = err.safe_message();
        // Timeout is safe to expose, but not the exact duration
        assert_eq!(safe_msg, "Operation timed out");
    }
}
