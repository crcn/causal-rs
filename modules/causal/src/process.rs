//! Emit future and process handle for event submission.

use anyhow::Result;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::reactor_queue::ReactorQueue;
use crate::types::QueueStatus;

/// Handle returned after an event is published.
pub struct ProcessHandle {
    pub correlation_id: Uuid,
    pub event_id: Uuid,
    pub(crate) queue: Option<Arc<dyn ReactorQueue>>,
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessHandle")
            .field("correlation_id", &self.correlation_id)
            .field("event_id", &self.event_id)
            .finish()
    }
}

impl ProcessHandle {
    /// Return a summary of pending work for this workflow.
    pub async fn status(&self) -> Result<QueueStatus> {
        match &self.queue {
            Some(queue) => queue.status(self.correlation_id).await,
            None => anyhow::bail!("ProcessHandle has no queue reference"),
        }
    }

    /// Cancel this workflow (best-effort stop-at-next-checkpoint).
    pub async fn cancel(&self) -> Result<()> {
        match &self.queue {
            Some(queue) => queue.cancel(self.correlation_id).await,
            None => anyhow::bail!("ProcessHandle has no queue reference"),
        }
    }
}

/// Type-erased async closure: Option<Uuid> → Result<ProcessHandle>
pub(crate) type PublishFn = Box<
    dyn FnOnce(Option<Uuid>) -> Pin<Box<dyn Future<Output = Result<ProcessHandle>> + Send>> + Send,
>;

/// Type-erased async closure: () → Result<()>
pub(crate) type SettleFn =
    Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send>;

/// Future returned by `Engine::emit()`.
///
/// Awaiting directly publishes the event (fire-and-forget).
/// Chain `.settled()` to also drive the full causal tree to completion.
pub struct EmitFuture {
    publish: Option<PublishFn>,
    settle: Option<SettleFn>,
    correlation_id: Option<Uuid>,
    task: Option<Pin<Box<dyn Future<Output = Result<ProcessHandle>> + Send>>>,
}

impl EmitFuture {
    pub(crate) fn new(
        publish: PublishFn,
        settle: SettleFn,
    ) -> Self {
        Self {
            publish: Some(publish),
            settle: Some(settle),
            correlation_id: None,
            task: None,
        }
    }

    /// Override the auto-generated correlation ID with a custom one.
    ///
    /// Useful for tying an engine emit to an external trace or request ID.
    ///
    /// ```ignore
    /// engine.emit(event).correlation_id(my_uuid).settled().await?;
    /// ```
    pub fn correlation_id(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Settle: publish event, then drive the entire causal tree to completion.
    ///
    /// Returns after all reactors (and their emitted events)
    /// have been fully processed.
    pub fn settled(self) -> SettleFuture {
        SettleFuture {
            publish: self.publish,
            settle: self.settle,
            correlation_id: self.correlation_id,
            timeout_duration: None,
            task: None,
        }
    }
}

impl Future for EmitFuture {
    type Output = Result<ProcessHandle>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();

        if this.task.is_none() {
            let publish = this
                .publish
                .take()
                .expect("EmitFuture polled after completion");
            let correlation_id = this.correlation_id;
            this.task = Some(publish(correlation_id));
        }

        this.task.as_mut().unwrap().as_mut().poll(cx)
    }
}

/// Future for synchronous settlement.
///
/// Two phases:
/// 1. Publish the event
/// 2. Drive settlement (process all pending events/effects)
///
/// Optionally wraps settlement with a wall-clock timeout via `.timeout()`.
pub struct SettleFuture {
    publish: Option<PublishFn>,
    settle: Option<SettleFn>,
    correlation_id: Option<Uuid>,
    timeout_duration: Option<Duration>,
    task: Option<Pin<Box<dyn Future<Output = Result<ProcessHandle>> + Send>>>,
}

impl SettleFuture {
    /// Set a wall-clock timeout for the settlement phase.
    ///
    /// If settlement takes longer than `duration`, returns an error.
    /// The publish phase is not subject to the timeout — only the
    /// settle loop (processing all pending events/effects).
    ///
    /// ```ignore
    /// engine.emit(event)
    ///     .settled()
    ///     .timeout(Duration::from_secs(30))
    ///     .await?;
    /// ```
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout_duration = Some(duration);
        self
    }
}

impl Future for SettleFuture {
    type Output = Result<ProcessHandle>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();

        if this.task.is_none() {
            let publish = this
                .publish
                .take()
                .expect("SettleFuture polled after completion");
            let settle = this
                .settle
                .take()
                .expect("SettleFuture polled after completion");
            let timeout_duration = this.timeout_duration;
            let correlation_id = this.correlation_id;
            this.task = Some(Box::pin(async move {
                let handle = publish(correlation_id).await?;
                if let Some(duration) = timeout_duration {
                    tokio::time::timeout(duration, settle())
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "Settlement timed out after {:.1}s",
                                duration.as_secs_f64()
                            )
                        })??;
                } else {
                    settle().await?;
                }
                Ok(handle)
            }));
        }

        this.task.as_mut().unwrap().as_mut().poll(cx)
    }
}
