//! Dispatch future and process handle for event submission.

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

/// Handle returned after an event is published.
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

/// Future returned by `Engine::dispatch()`.
///
/// Awaiting directly publishes the event (fire-and-forget).
/// Chain `.settled()` to also drive the full causal tree to completion.
pub struct DispatchFuture {
    publish: Option<PublishFn>,
    settle: Option<SettleFn>,
    task: Option<Pin<Box<dyn Future<Output = Result<ProcessHandle>> + Send>>>,
}

impl DispatchFuture {
    pub(crate) fn new(publish: PublishFn, settle: SettleFn) -> Self {
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
            task: None,
        }
    }
}

impl Future for DispatchFuture {
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
                .expect("DispatchFuture polled after completion");
            this.task = Some(publish());
        }

        this.task.as_mut().unwrap().as_mut().poll(cx)
    }
}

/// Future for synchronous settlement.
///
/// Two phases:
/// 1. Publish the event
/// 2. Drive settlement (process all pending events/effects)
pub struct SettleFuture {
    publish: Option<PublishFn>,
    settle: Option<SettleFn>,
    task: Option<Pin<Box<dyn Future<Output = Result<ProcessHandle>> + Send>>>,
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
            this.task = Some(Box::pin(async move {
                let handle = publish().await?;
                settle().await?;
                Ok(handle)
            }));
        }

        this.task.as_mut().unwrap().as_mut().poll(cx)
    }
}
