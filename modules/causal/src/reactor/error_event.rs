use std::any::{Any, TypeId};
use std::sync::Arc;

/// Event emitted when a reactor returns an error.
///
/// Minimal data structure - just what happened and what failed.
/// Users write explicit handling logic for retries, compensation, etc.
#[derive(Clone)]
pub struct ReactorError {
    /// The event that triggered the reactor.
    pub source_event: Arc<dyn Any + Send + Sync>,

    /// TypeId of the source event (for filtering).
    pub source_event_type: TypeId,

    /// The error value (preserves anyhow::Error for downcast).
    pub error: Arc<anyhow::Error>,
}

impl ReactorError {
    pub fn new(
        source_event: Arc<dyn Any + Send + Sync>,
        source_event_type: TypeId,
        error: anyhow::Error,
    ) -> Self {
        Self {
            source_event,
            source_event_type,
            error: Arc::new(error),
        }
    }

    /// Downcast error to specific type for typed error handling.
    pub fn downcast<E: std::error::Error + Send + Sync + 'static>(&self) -> Option<&E> {
        self.error.downcast_ref::<E>()
    }
}

impl std::fmt::Debug for ReactorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReactorError")
            .field("source_event_type", &self.source_event_type)
            .field("error", &self.error.to_string())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct TestEvent;

    #[derive(Debug, thiserror::Error)]
    #[error("custom error: {message}")]
    struct CustomError {
        message: String,
    }

    #[test]
    fn test_handler_error_creation() {
        let event = TestEvent;
        let error = anyhow::anyhow!("test error");

        let handler_error =
            ReactorError::new(Arc::new(event.clone()), TypeId::of::<TestEvent>(), error);

        assert_eq!(handler_error.source_event_type, TypeId::of::<TestEvent>());
        assert!(handler_error.error.to_string().contains("test error"));
    }

    #[test]
    fn test_downcast_success() {
        let event = TestEvent;
        let custom_error = CustomError {
            message: "specific error".to_string(),
        };
        let error = anyhow::Error::from(custom_error);

        let handler_error = ReactorError::new(Arc::new(event), TypeId::of::<TestEvent>(), error);

        // Should successfully downcast to CustomError
        let downcasted = handler_error.downcast::<CustomError>();
        assert!(downcasted.is_some());
        assert_eq!(downcasted.unwrap().message, "specific error");
    }

    #[test]
    fn test_downcast_failure() {
        let event = TestEvent;
        let error = anyhow::anyhow!("generic error");

        let handler_error = ReactorError::new(Arc::new(event), TypeId::of::<TestEvent>(), error);

        // Should fail to downcast to CustomError
        let downcasted = handler_error.downcast::<CustomError>();
        assert!(downcasted.is_none());
    }

    #[test]
    fn test_debug_formatting() {
        let event = TestEvent;
        let error = anyhow::anyhow!("formatting test");

        let handler_error = ReactorError::new(Arc::new(event), TypeId::of::<TestEvent>(), error);

        let debug_str = format!("{:?}", handler_error);
        assert!(debug_str.contains("ReactorError"));
        assert!(debug_str.contains("formatting test"));
    }

    #[test]
    fn test_clone() {
        let event = TestEvent;
        let error = anyhow::anyhow!("clone test");

        let handler_error = ReactorError::new(Arc::new(event), TypeId::of::<TestEvent>(), error);

        let cloned = handler_error.clone();
        assert_eq!(cloned.source_event_type, handler_error.source_event_type);
        assert_eq!(cloned.error.to_string(), handler_error.error.to_string());
    }
}
