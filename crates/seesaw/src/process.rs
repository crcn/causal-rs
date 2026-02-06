//! Process future and handle for queue-backed event processing

use anyhow::Result;
use futures::StreamExt;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use uuid::Uuid;

use crate::Store;

/// Future returned by Engine::process()
///
/// Lazy: doesn't publish until polled
pub struct ProcessFuture<St: Store> {
    store: Arc<St>,
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

impl<St: Store> ProcessFuture<St> {
    pub(crate) fn new(
        store: Arc<St>,
        event_id: Uuid,
        correlation_id: Uuid,
        parent_id: Option<Uuid>,
        event_type: String,
        payload: serde_json::Value,
        hops: i32,
    ) -> Self {
        Self {
            store,
            event_id,
            correlation_id,
            parent_id,
            event_type,
            payload,
            hops,
            state: ProcessState::Init,
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
        let ProcessFuture {
            store,
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
                    let mut stream = store.subscribe_workflow_events(correlation_id).await?;

                    // Publish only after LISTEN is established so terminal events
                    // emitted quickly after processing are not missed.
                    let queued_event = crate::QueuedEvent {
                        id: 0,
                        event_id,
                        parent_id,
                        correlation_id,
                        event_type,
                        payload,
                        hops,
                        batch_id: None,
                        batch_index: None,
                        batch_size: None,
                        created_at: chrono::Utc::now(),
                    };
                    store.publish(queued_event).await?;

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
        }
    }
}

impl<St: Store> Future for ProcessFuture<St> {
    type Output = Result<ProcessHandle>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match &mut this.state {
                ProcessState::Init => {
                    let store = this.store.clone();
                    let queued_event = crate::QueuedEvent {
                        id: 0, // Will be set by database
                        event_id: this.event_id,
                        parent_id: this.parent_id,
                        correlation_id: this.correlation_id,
                        event_type: this.event_type.clone(),
                        payload: this.payload.clone(),
                        hops: this.hops,
                        batch_id: None,
                        batch_index: None,
                        batch_size: None,
                        created_at: chrono::Utc::now(),
                    };

                    this.state = ProcessState::Publishing(Box::pin(async move {
                        store.publish(queued_event).await
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
pub struct WaitFuture<T, F> {
    timeout: Option<Duration>,
    init: Option<WaitInit<T>>,
    task: Option<WaitTask<T>>,
    _marker: std::marker::PhantomData<T>,
    _matcher: std::marker::PhantomData<F>,
}

impl<T, F> Unpin for WaitFuture<T, F> {}

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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::channel::mpsc;
    use futures::Stream;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    struct TestStore {
        publish_calls: AtomicUsize,
        subscribe_calls: AtomicUsize,
        hold_publish: AtomicBool,
        fail_subscribe: AtomicBool,
        gate: Arc<Notify>,
        call_order: Mutex<Vec<&'static str>>,
        emit_on_publish: Mutex<Option<crate::WorkflowEvent>>,
        stream_tx: Mutex<Option<mpsc::UnboundedSender<crate::WorkflowEvent>>>,
    }

    impl TestStore {
        fn new() -> Self {
            Self {
                publish_calls: AtomicUsize::new(0),
                subscribe_calls: AtomicUsize::new(0),
                hold_publish: AtomicBool::new(true),
                fail_subscribe: AtomicBool::new(false),
                gate: Arc::new(Notify::new()),
                call_order: Mutex::new(Vec::new()),
                emit_on_publish: Mutex::new(None),
                stream_tx: Mutex::new(None),
            }
        }

        fn publish_calls(&self) -> usize {
            self.publish_calls.load(Ordering::SeqCst)
        }

        fn set_hold_publish(&self, hold: bool) {
            self.hold_publish.store(hold, Ordering::SeqCst);
            if !hold {
                self.gate.notify_waiters();
            }
        }

        fn release_publish(&self) {
            self.gate.notify_waiters();
        }

        fn set_fail_subscribe(&self, should_fail: bool) {
            self.fail_subscribe.store(should_fail, Ordering::SeqCst);
        }

        fn set_emit_on_publish(&self, event: crate::WorkflowEvent) {
            *self.emit_on_publish.lock() = Some(event);
        }

        fn call_order(&self) -> Vec<&'static str> {
            self.call_order.lock().clone()
        }

        fn subscribe_calls(&self) -> usize {
            self.subscribe_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl crate::Store for TestStore {
        async fn publish(&self, _event: crate::QueuedEvent) -> Result<()> {
            self.publish_calls.fetch_add(1, Ordering::SeqCst);
            self.call_order.lock().push("publish");

            if let Some(event) = self.emit_on_publish.lock().clone() {
                if let Some(tx) = self.stream_tx.lock().as_mut() {
                    let _ = tx.unbounded_send(event);
                }
            }

            if self.hold_publish.load(Ordering::SeqCst) {
                self.gate.notified().await;
            }
            Ok(())
        }

        async fn poll_next(&self) -> Result<Option<crate::QueuedEvent>> {
            Ok(None)
        }

        async fn ack(&self, _id: i64) -> Result<()> {
            Ok(())
        }

        async fn nack(&self, _id: i64, _retry_after_secs: u64) -> Result<()> {
            Ok(())
        }

        async fn load_state<S>(&self, _correlation_id: Uuid) -> Result<Option<(S, i32)>>
        where
            S: for<'de> serde::Deserialize<'de> + Send,
        {
            Ok(None)
        }

        async fn save_state<S>(
            &self,
            _correlation_id: Uuid,
            _state: &S,
            _expected_version: i32,
        ) -> Result<i32>
        where
            S: serde::Serialize + Send + Sync,
        {
            Ok(1)
        }

        async fn insert_effect_intent(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _correlation_id: Uuid,
            _event_type: String,
            _event_payload: serde_json::Value,
            _parent_event_id: Option<Uuid>,
            _batch_id: Option<Uuid>,
            _batch_index: Option<i32>,
            _batch_size: Option<i32>,
            _execute_at: chrono::DateTime<chrono::Utc>,
            _timeout_seconds: i32,
            _max_attempts: i32,
            _priority: i32,
        ) -> Result<()> {
            Ok(())
        }

        async fn poll_next_effect(&self) -> Result<Option<crate::QueuedEffectExecution>> {
            Ok(None)
        }

        async fn complete_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _result: serde_json::Value,
        ) -> Result<()> {
            Ok(())
        }

        async fn complete_effect_with_events(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _result: serde_json::Value,
            _emitted_events: Vec<crate::EmittedEvent>,
        ) -> Result<()> {
            Ok(())
        }

        async fn fail_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _error: String,
            _attempts: i32,
        ) -> Result<()> {
            Ok(())
        }

        async fn dlq_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _error: String,
            _reason: String,
            _attempts: i32,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_workflow_status(
            &self,
            _correlation_id: Uuid,
        ) -> Result<crate::WorkflowStatus> {
            Ok(crate::WorkflowStatus {
                correlation_id: _correlation_id,
                state: None,
                pending_effects: 0,
                is_settled: true,
                last_event: None,
            })
        }

        async fn subscribe_workflow_events(
            &self,
            _correlation_id: Uuid,
        ) -> Result<Box<dyn Stream<Item = crate::WorkflowEvent> + Send + Unpin>> {
            if self.fail_subscribe.load(Ordering::SeqCst) {
                anyhow::bail!("subscribe failed");
            }

            self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
            self.call_order.lock().push("subscribe");
            let (tx, rx) = mpsc::unbounded();
            *self.stream_tx.lock() = Some(tx);
            Ok(Box::new(rx))
        }
    }

    fn create_process_future(store: Arc<TestStore>) -> ProcessFuture<TestStore> {
        ProcessFuture::new(
            store,
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            "TestEvent".to_string(),
            serde_json::json!({}),
            0,
        )
    }

    #[tokio::test]
    async fn process_future_does_not_spawn_multiple_publishes_while_pending() {
        let store = Arc::new(TestStore::new());
        let mut fut = Box::pin(create_process_future(store.clone()));

        assert!(matches!(futures::poll!(fut.as_mut()), Poll::Pending));
        tokio::task::yield_now().await;

        assert!(matches!(futures::poll!(fut.as_mut()), Poll::Pending));
        tokio::task::yield_now().await;

        // Must still be a single publish attempt for one ProcessFuture.
        assert_eq!(store.publish_calls(), 1);

        store.release_publish();
    }

    #[tokio::test]
    async fn process_future_publishes_once_on_completion() {
        let store = Arc::new(TestStore::new());
        let event_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();
        let fut = ProcessFuture::new(
            store.clone(),
            event_id,
            correlation_id,
            None,
            "TestEvent".to_string(),
            serde_json::json!({}),
            0,
        );

        let gate = store.gate.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            gate.notify_waiters();
        });

        let handle = fut.await.expect("process future should complete");
        assert_eq!(handle.event_id, event_id);
        assert_eq!(handle.correlation_id, correlation_id);
        assert_eq!(store.publish_calls(), 1);
    }

    #[tokio::test]
    async fn wait_future_subscribes_before_publish_and_returns_match() {
        let store = Arc::new(TestStore::new());
        store.set_hold_publish(false);

        let correlation_id = Uuid::new_v4();
        store.set_emit_on_publish(crate::WorkflowEvent {
            event_id: Uuid::new_v4(),
            correlation_id,
            event_type: "TerminalEvent".to_string(),
            payload: serde_json::json!({ "ok": true }),
        });

        let process = ProcessFuture::new(
            store.clone(),
            Uuid::new_v4(),
            correlation_id,
            None,
            "StartEvent".to_string(),
            serde_json::json!({}),
            0,
        );

        let result = process
            .wait(|event| {
                event.downcast_ref::<crate::WorkflowEvent>().and_then(|e| {
                    match e.event_type.as_str() {
                        "TerminalEvent" => Some(Ok(e.payload["ok"].as_bool().unwrap_or(false))),
                        _ => None,
                    }
                })
            })
            .timeout(Duration::from_millis(250))
            .await
            .expect("wait should resolve");

        assert!(result);
        assert_eq!(store.subscribe_calls(), 1);
        assert_eq!(store.publish_calls(), 1);
        assert_eq!(store.call_order(), vec!["subscribe", "publish"]);
    }

    #[tokio::test]
    async fn wait_future_times_out_without_matching_event() {
        let store = Arc::new(TestStore::new());
        store.set_hold_publish(false);

        let process = create_process_future(store.clone());
        let result: Result<()> = process
            .wait(|_event| None)
            .timeout(Duration::from_millis(50))
            .await;

        assert!(result.is_err(), "wait should timeout when no event arrives");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Timed out waiting for matching workflow event"),
            "unexpected timeout error message: {}",
            err
        );
    }

    #[tokio::test]
    async fn wait_future_propagates_subscribe_error_without_publishing() {
        let store = Arc::new(TestStore::new());
        store.set_hold_publish(false);
        store.set_fail_subscribe(true);

        let process = create_process_future(store.clone());
        let result: Result<()> = process.wait(|_event| None).await;

        assert!(result.is_err(), "subscribe error should bubble from wait");
        assert!(result.unwrap_err().to_string().contains("subscribe failed"));
        assert_eq!(store.subscribe_calls(), 0);
        assert_eq!(store.publish_calls(), 0);
        assert!(store.call_order().is_empty());
    }
}
