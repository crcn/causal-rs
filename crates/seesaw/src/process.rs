//! Process future and handle for Backend-backed event processing

use anyhow::Result;
use futures::StreamExt;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use uuid::Uuid;

use crate::backend::{Backend, DispatchedEvent};
use crate::backend::capability::WorkflowSubscriptionBackend;

/// Future returned by Engine::dispatch()
///
/// Lazy: doesn't publish until polled
pub struct ProcessFuture<B: Backend> {
    backend: Arc<B>,
    event_id: Uuid,
    correlation_id: Uuid,
    parent_id: Option<Uuid>,
    event_type: String,
    payload: serde_json::Value,
    hops: i32,
    state: ProcessState,
}

enum ProcessState {
    Init,
    Publishing(Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>),
    Done,
}

impl<B: Backend> ProcessFuture<B> {
    pub(crate) fn new(
        backend: Arc<B>,
        event_id: Uuid,
        correlation_id: Uuid,
        parent_id: Option<Uuid>,
        event_type: String,
        payload: serde_json::Value,
        hops: i32,
    ) -> Self {
        Self {
            backend,
            event_id,
            correlation_id,
            parent_id,
            event_type,
            payload,
            hops,
            state: ProcessState::Init,
        }
    }

    /// Set custom event ID (for idempotency)
    pub fn with_id(mut self, event_id: Uuid) -> Self {
        self.event_id = event_id;
        self
    }

    /// Set workflow/correlation ID
    pub fn with_workflow_id(mut self, correlation_id: Uuid) -> Self {
        self.correlation_id = correlation_id;
        self
    }

    /// Set parent event ID (for causation tracking)
    pub fn with_parent(mut self, parent_id: Uuid) -> Self {
        self.parent_id = Some(parent_id);
        self
    }
}

// .wait() is only available when backend supports subscriptions
impl<B> ProcessFuture<B>
where
    B: WorkflowSubscriptionBackend,
{
    /// Wait for terminal event matching closure (requires WorkflowSubscriptionBackend).
    ///
    /// Closure returns:
    /// - None = not a terminal event, keep waiting
    /// - Some(Ok(value)) = success, return value
    /// - Some(Err(error)) = failure, propagate error
    pub fn wait<T, F>(self, matcher: F) -> WaitFuture<T, F, B>
    where
        F: Fn(&dyn std::any::Any) -> Option<Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let ProcessFuture {
            backend,
            event_id,
            correlation_id,
            parent_id,
            event_type,
            payload,
            hops,
            ..
        } = self;

        WaitFuture {
            timeout: None,
            init: Some(Box::new(move |timeout| {
                Box::pin(async move {
                    let mut stream = backend.subscribe_workflow_events(correlation_id).await?;

                    // Publish only after LISTEN is established so terminal events
                    // emitted quickly after processing are not missed.
                    let dispatched_event = DispatchedEvent {
                        event_id,
                        parent_id,
                        correlation_id,
                        event_type,
                        payload,
                        hops,
                        retry_count: 0,
                        batch_id: None,
                        batch_index: None,
                        batch_size: None,
                        created_at: chrono::Utc::now(),
                    };
                    backend.publish(dispatched_event).await?;

                    let wait_for_match = async move {
                        while let Some(event) = stream.next().await {
                            if let Some(result) = matcher(&event as &dyn Any) {
                                return result;
                            }
                            if let Some(result) = matcher(&event.payload as &dyn Any) {
                                return result;
                            }
                        }

                        anyhow::bail!(
                            "Workflow event stream ended before a matching terminal event was received"
                        );
                    };

                    match timeout {
                        Some(duration) => {
                            match tokio::time::timeout(duration, wait_for_match).await {
                                Ok(result) => result,
                                Err(_) => {
                                    anyhow::bail!(
                                        "Timed out waiting for matching workflow event after {:?}",
                                        duration
                                    )
                                }
                            }
                        }
                        None => wait_for_match.await,
                    }
                })
            })),
            task: None,
            _marker: std::marker::PhantomData,
            _matcher: std::marker::PhantomData,
            _backend: std::marker::PhantomData,
        }
    }
}

impl<B: Backend> Future for ProcessFuture<B> {
    type Output = Result<ProcessHandle>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match &mut this.state {
                ProcessState::Init => {
                    let backend = this.backend.clone();
                    let dispatched_event = DispatchedEvent {
                        event_id: this.event_id,
                        parent_id: this.parent_id,
                        correlation_id: this.correlation_id,
                        event_type: this.event_type.clone(),
                        payload: this.payload.clone(),
                        hops: this.hops,
                        retry_count: 0,
                        batch_id: None,
                        batch_index: None,
                        batch_size: None,
                        created_at: chrono::Utc::now(),
                    };

                    this.state = ProcessState::Publishing(Box::pin(async move {
                        backend.publish(dispatched_event).await
                    }));
                }
                ProcessState::Publishing(fut) => match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        this.state = ProcessState::Done;
                        return Poll::Ready(Ok(ProcessHandle {
                            correlation_id: this.correlation_id,
                            event_id: this.event_id,
                        }));
                    }
                    Poll::Ready(Err(e)) => {
                        this.state = ProcessState::Done;
                        return Poll::Ready(Err(e));
                    }
                },
                ProcessState::Done => {
                    panic!("ProcessFuture polled after completion");
                }
            }
        }
    }
}

/// Handle returned after event is published
pub struct ProcessHandle {
    pub correlation_id: Uuid,
    pub event_id: Uuid,
}

type WaitTask<T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;
type WaitInit<T> = Box<dyn FnOnce(Option<Duration>) -> WaitTask<T> + Send + 'static>;

/// Future for waiting on terminal events
pub struct WaitFuture<T, F, B> {
    timeout: Option<Duration>,
    init: Option<WaitInit<T>>,
    task: Option<WaitTask<T>>,
    _marker: std::marker::PhantomData<T>,
    _matcher: std::marker::PhantomData<F>,
    _backend: std::marker::PhantomData<B>,
}

impl<T, F, B> Unpin for WaitFuture<T, F, B> {}

impl<T, F, B> WaitFuture<T, F, B>
where
    B: WorkflowSubscriptionBackend,
    F: Fn(&dyn std::any::Any) -> Option<Result<T>> + Send + 'static,
    T: Send + 'static,
{
    /// Set timeout for waiting
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
    }
}

impl<T, F, B> Future for WaitFuture<T, F, B>
where
    B: WorkflowSubscriptionBackend,
    F: Fn(&dyn std::any::Any) -> Option<Result<T>> + Send + 'static,
    T: Send + 'static,
{
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.task.is_none() {
            let init = this
                .init
                .take()
                .expect("WaitFuture polled after completion");
            let timeout = this.timeout.take();
            this.task = Some(init(timeout));
        }

        let task = this
            .task
            .as_mut()
            .expect("WaitFuture task should be initialized");

        match task.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.task = None;
                Poll::Ready(result)
            }
        }
    }
}

// ── DispatchFuture / SettleFuture ────────────────────────────────────────

/// Type-erased settle driver closure.
pub type SettleDriver = Arc<
    dyn Fn(Uuid) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync,
>;

/// Future returned by `Engine::process()` (when `B: SettleableBackend`).
///
/// Wraps `ProcessFuture` and adds `.settled()` for synchronous settlement.
/// Can also be awaited directly (fire-and-forget, same as `dispatch()`).
pub struct DispatchFuture<B: Backend> {
    inner: ProcessFuture<B>,
    settle_driver: Option<SettleDriver>,
}

impl<B: Backend> DispatchFuture<B> {
    pub(crate) fn new(inner: ProcessFuture<B>, settle_driver: Option<SettleDriver>) -> Self {
        Self {
            inner,
            settle_driver,
        }
    }

    /// Set custom event ID (for idempotency).
    pub fn with_id(mut self, event_id: Uuid) -> Self {
        self.inner = self.inner.with_id(event_id);
        self
    }

    /// Set workflow/correlation ID.
    pub fn with_workflow_id(mut self, correlation_id: Uuid) -> Self {
        self.inner = self.inner.with_workflow_id(correlation_id);
        self
    }

    /// Set parent event ID (for causation tracking).
    pub fn with_parent(mut self, parent_id: Uuid) -> Self {
        self.inner = self.inner.with_parent(parent_id);
        self
    }

    /// Settle: publish event, then drive the entire causal tree to completion.
    ///
    /// Returns after all inline and queued handlers (and their emitted events)
    /// have been fully processed.
    pub fn settled(self) -> SettleFuture<B> {
        SettleFuture {
            inner: self.inner,
            settle_driver: self.settle_driver,
            settle_task: None,
            handle: None,
        }
    }
}

// Forward .wait() for WorkflowSubscriptionBackend
impl<B> DispatchFuture<B>
where
    B: WorkflowSubscriptionBackend,
{
    /// Wait for terminal event matching closure (requires WorkflowSubscriptionBackend).
    pub fn wait<T, F>(self, matcher: F) -> WaitFuture<T, F, B>
    where
        F: Fn(&dyn std::any::Any) -> Option<Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.inner.wait(matcher)
    }
}

// Awaiting DispatchFuture directly delegates to ProcessFuture (fire-and-forget).
impl<B: Backend> Future for DispatchFuture<B> {
    type Output = Result<ProcessHandle>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll(cx)
    }
}

/// Future for synchronous settlement.
///
/// Two phases:
/// 1. Publish the event (poll inner ProcessFuture)
/// 2. Drive settlement (call settle_driver with correlation_id)
pub struct SettleFuture<B: Backend> {
    inner: ProcessFuture<B>,
    settle_driver: Option<SettleDriver>,
    settle_task: Option<Pin<Box<dyn Future<Output = Result<()>> + Send>>>,
    handle: Option<ProcessHandle>,
}

impl<B: Backend> Future for SettleFuture<B> {
    type Output = Result<ProcessHandle>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Phase 1: publish event
        if this.handle.is_none() {
            match Pin::new(&mut this.inner).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(handle)) => {
                    // Start phase 2
                    if let Some(driver) = this.settle_driver.take() {
                        this.settle_task = Some(driver(handle.correlation_id));
                    }
                    this.handle = Some(handle);
                }
            }
        }

        // Phase 2: drive settlement
        if let Some(task) = this.settle_task.as_mut() {
            match task.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {
                    this.settle_task = None;
                    return Poll::Ready(Ok(this.handle.take().unwrap()));
                }
            }
        }

        // No settle driver — just return the handle
        Poll::Ready(Ok(this.handle.take().unwrap()))
    }
}
