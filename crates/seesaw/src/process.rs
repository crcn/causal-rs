//! Emit future and process handle for event submission.

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use uuid::Uuid;

/// Handle returned after an event is published.
#[derive(Debug)]
pub struct ProcessHandle {
    pub correlation_id: Uuid,
    pub event_id: Uuid,
}

/// Type-erased async closure: () → Result<ProcessHandle>
pub(crate) type PublishFn =
    Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Result<ProcessHandle>> + Send>> + Send>;

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
            task: None,
        }
    }

    /// Settle: publish event, then drive the entire causal tree to completion.
    ///
    /// Returns after all inline and queued handlers (and their emitted events)
    /// have been fully processed.
    pub fn settled(self) -> SettleFuture {
        SettleFuture {
            publish: self.publish,
            settle: self.settle,
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
            this.task = Some(publish());
        }

        this.task.as_mut().unwrap().as_mut().poll(cx)
    }
}

/// Deprecated: use `EmitFuture` instead.
#[deprecated(note = "renamed to EmitFuture")]
pub type DispatchFuture = EmitFuture;

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
            this.task = Some(Box::pin(async move {
                let handle = publish().await?;
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
