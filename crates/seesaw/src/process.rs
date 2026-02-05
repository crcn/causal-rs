//! Process future and handle for queue-backed event processing

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::Store;

/// Future returned by Engine::process()
///
/// Lazy: doesn't publish until polled
pub struct ProcessFuture<St: Store> {
    store: Arc<St>,
    event_id: Uuid,
    saga_id: Uuid,
    parent_id: Option<Uuid>,
    event_type: String,
    payload: serde_json::Value,
    hops: i32,
}

impl<St: Store> ProcessFuture<St> {
    pub(crate) fn new(
        store: Arc<St>,
        event_id: Uuid,
        saga_id: Uuid,
        parent_id: Option<Uuid>,
        event_type: String,
        payload: serde_json::Value,
        hops: i32,
    ) -> Self {
        Self {
            store,
            event_id,
            saga_id,
            parent_id,
            event_type,
            payload,
            hops,
        }
    }

    /// Wait for terminal event matching closure
    ///
    /// Closure returns:
    /// - None = not a terminal event, keep waiting
    /// - Some(Ok(value)) = success, return value
    /// - Some(Err(error)) = failure, propagate error
    pub fn wait<T, F>(self, matcher: F) -> WaitFuture<T, F>
    where
        F: Fn(&dyn std::any::Any) -> Option<Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        WaitFuture {
            saga_id: self.saga_id,
            matcher,
            timeout: None,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<St: Store> Future for ProcessFuture<St> {
    type Output = Result<ProcessHandle>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Publish event to store
        let store = self.store.clone();
        let queued_event = crate::QueuedEvent {
            id: 0, // Will be set by database
            event_id: self.event_id,
            parent_id: self.parent_id,
            saga_id: self.saga_id,
            event_type: self.event_type.clone(),
            payload: self.payload.clone(),
            hops: self.hops,
            created_at: chrono::Utc::now(),
        };

        // Spawn publish task
        let (tx, mut rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = store.publish(queued_event).await;
            let _ = tx.send(result);
        });

        // Wait for publish to complete
        match rx.try_recv() {
            Ok(Ok(())) => Poll::Ready(Ok(ProcessHandle {
                saga_id: self.saga_id,
                event_id: self.event_id,
            })),
            Ok(Err(e)) => Poll::Ready(Err(e)),
            Err(_) => {
                // Still pending
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

/// Handle returned after event is published
pub struct ProcessHandle {
    pub saga_id: Uuid,
    pub event_id: Uuid,
}

/// Future for waiting on terminal events
pub struct WaitFuture<T, F> {
    saga_id: Uuid,
    matcher: F,
    timeout: Option<Duration>,
    _marker: std::marker::PhantomData<T>,
}

impl<T, F> WaitFuture<T, F>
where
    F: Fn(&dyn std::any::Any) -> Option<Result<T>> + Send + 'static,
    T: Send + 'static,
{
    /// Set timeout for waiting
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
    }
}

impl<T, F> Future for WaitFuture<T, F>
where
    F: Fn(&dyn std::any::Any) -> Option<Result<T>> + Send + 'static,
    T: Send + 'static,
{
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // TODO: Implement LISTEN/NOTIFY subscription
        // For now, return pending
        // This will be implemented when we add the runtime
        Poll::Pending
    }
}
