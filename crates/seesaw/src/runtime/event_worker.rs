//! Event Worker - polls events and executes inline/queued effects.

use anyhow::Result;
use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::handler::Context;
use crate::handler_registry::HandlerRegistry;
use crate::queue_backend::{QueueBackend, StoreQueueBackend};
use crate::{
    EventProcessingCommit, InlineHandlerFailure, QueuedEvent, QueuedHandlerIntent, Store,
    NAMESPACE_SEESAW,
};

/// Event worker configuration.
#[derive(Debug, Clone)]
pub struct EventWorkerConfig {
    /// Polling interval when no events available.
    pub poll_interval: Duration,
    /// Maximum hop count before DLQ (infinite loop detection).
    pub max_hops: i32,
    /// Maximum number of events an effect may emit in one batch.
    pub max_batch_size: usize,
    /// Maximum retry count for event-level failures in inline processing path.
    pub max_inline_retry_attempts: i32,
}

impl Default for EventWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            max_hops: 50,
            max_batch_size: 10_000,
            max_inline_retry_attempts: 3,
        }
    }
}

/// Event worker - polls and processes events.
pub struct EventWorker<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
    queue_backend: Arc<dyn QueueBackend<St>>,
    config: EventWorkerConfig,
    shutdown: Arc<AtomicBool>,
}

impl<D, St> EventWorker<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    pub(crate) fn new(
        store: Arc<St>,
        deps: Arc<D>,
        effects: Arc<HandlerRegistry<D>>,
        config: EventWorkerConfig,
    ) -> Self {
        Self {
            store,
            deps,
            effects,
            queue_backend: Arc::new(StoreQueueBackend),
            config,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn with_queue_backend(self, queue_backend: Arc<dyn QueueBackend<St>>) -> Self {
        Self {
            queue_backend,
            ..self
        }
    }

    pub(crate) fn with_shutdown(self, shutdown: Arc<AtomicBool>) -> Self {
        Self { shutdown, ..self }
    }

    /// Run worker loop (polls events and processes them).
    pub async fn run(self) -> Result<()> {
        info!("Event worker started");

        while !self.shutdown.load(Ordering::SeqCst) {
            match self.process_next_event().await {
                Ok(processed) => {
                    if !processed {
                        sleep(self.config.poll_interval).await;
                    }
                }
                Err(e) => {
                    error!("Error processing event: {}", e);
                    sleep(self.config.poll_interval).await;
                }
            }
        }

        info!("Event worker stopped");
        Ok(())
    }

    /// Process next available event.
    ///
    /// Returns true if event was processed, false if no events available.
    async fn process_next_event(&self) -> Result<bool> {
        let Some(event) = self.store.poll_next().await? else {
            return Ok(false);
        };

        if let Err(error) = self.process_claimed_event(&event).await {
            warn!(
                "Event processing failed, nacking for retry: event_id={}, error={}",
                event.event_id, error
            );
            let _ = self.store.nack(event.id, 1).await;
            return Err(error);
        }

        Ok(true)
    }

    async fn process_claimed_event(&self, event: &QueuedEvent) -> Result<()> {
        info!(
            "Processing event: type={}, workflow={}, hops={}",
            event.event_type, event.correlation_id, event.hops
        );

        if event.hops >= self.config.max_hops {
            warn!(
                "Event exceeded max hops ({}), sending to DLQ: event_id={}",
                self.config.max_hops, event.event_id
            );
            let error = format!(
                "Event exceeded maximum hop count ({}) - infinite loop detected",
                self.config.max_hops
            );
            self.store
                .dlq_effect(
                    event.event_id,
                    "__event_max_hops__".to_string(),
                    error,
                    "infinite_loop".to_string(),
                    event.hops,
                )
                .await?;
            self.store.ack(event.id).await?;
            return Ok(());
        }

        if event.retry_count >= self.config.max_inline_retry_attempts {
            warn!(
                "Event exceeded max retry attempts ({}), sending to DLQ: event_id={}",
                self.config.max_inline_retry_attempts, event.event_id
            );
            let error = format!("Event failed after {} retry attempts", event.retry_count);
            self.store
                .dlq_effect(
                    event.event_id,
                    "__inline_effect_retry_exhausted__".to_string(),
                    error,
                    "max_retries_exceeded".to_string(),
                    event.retry_count,
                )
                .await?;
            self.store.ack(event.id).await?;
            return Ok(());
        }

        let (typed_event, event_type_id) = self.decode_event(&event.event_type, &event.payload)?;

        let matching_effects: Vec<_> = self
            .effects
            .all()
            .into_iter()
            .filter(|effect| effect.can_handle(event_type_id))
            .collect();

        let mut queued_effect_intents = Vec::new();
        for effect in matching_effects.iter().filter(|effect| !effect.is_inline()) {
            let execute_at = match effect.delay {
                Some(delay) => {
                    chrono::Utc::now()
                        + chrono::Duration::from_std(delay)
                            .map_err(|_| anyhow::anyhow!("invalid queued effect delay"))?
                }
                None => chrono::Utc::now(),
            };
            let timeout_seconds = effect
                .timeout
                .map(|d| d.as_secs() as i32)
                .unwrap_or(30)
                .max(1);
            queued_effect_intents.push(QueuedHandlerIntent {
                handler_id: effect.id.clone(),
                parent_event_id: Some(event.event_id),
                batch_id: event.batch_id,
                batch_index: event.batch_index,
                batch_size: event.batch_size,
                execute_at,
                timeout_seconds,
                max_attempts: effect.max_attempts as i32,
                priority: effect.priority.unwrap_or(10),
                join_window_timeout_seconds: effect
                    .join_window_timeout
                    .map(|d| d.as_secs() as i32)
                    .map(|seconds| seconds.max(1)),
            });
        }

        let mut inline_effect_failures = Vec::new();
        let mut emitted_events = Vec::new();
        let mut inline_effects: Vec<_> = matching_effects.iter().filter(|effect| effect.is_inline()).collect();
        inline_effects.sort_by_key(|e| e.priority.unwrap_or(i32::MAX));
        for effect in inline_effects {
            match self
                .run_inline_effect(effect, event, typed_event.clone(), event_type_id)
                .await
            {
                Ok(mut emitted) => emitted_events.append(&mut emitted),
                Err(error) => {
                    let error_string = error.to_string();
                    warn!(
                        "Inline effect failed and will be persisted to DLQ: event_id={}, effect_id={}, error={}",
                        event.event_id, effect.id, error_string
                    );
                    inline_effect_failures.push(InlineHandlerFailure {
                        handler_id: effect.id.clone(),
                        error: error_string,
                        reason: "inline_failed".to_string(),
                        attempts: event.retry_count.saturating_add(1),
                    });
                }
            }
        }

        let inline_failure_count = inline_effect_failures.len();

        let queued_effect_ids = queued_effect_intents
            .iter()
            .map(|intent| intent.handler_id.clone())
            .collect::<Vec<_>>();

        self.store
            .commit_event_processing(EventProcessingCommit {
                event_row_id: event.id,
                event_id: event.event_id,
                correlation_id: event.correlation_id,
                event_type: event.event_type.clone(),
                event_payload: event.payload.clone(),
                queued_effect_intents,
                inline_effect_failures,
                emitted_events,
            })
            .await?;

        info!(
            "Event processing committed: workflow={}, event={}",
            event.correlation_id, event.event_id
        );

        for effect_id in queued_effect_ids {
            if let Err(error) = self
                .queue_backend
                .on_effect_intent_inserted(&*self.store, event.event_id, &effect_id)
                .await
            {
                warn!(
                    "Queue backend notification failed; relying on store polling fallback: event_id={}, effect_id={}, error={}",
                    event.event_id, effect_id, error
                );
            }
        }

        if inline_failure_count > 0 {
            warn!(
                "Event processed with inline failures moved to DLQ: event_id={}, inline_failures={}",
                event.event_id, inline_failure_count
            );
        } else {
            info!("Event processed successfully: event_id={}", event.event_id);
        }

        Ok(())
    }

    fn decode_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(Arc<dyn Any + Send + Sync>, TypeId)> {
        let codec = self.effects.find_codec_by_event_type(event_type);

        if let Some(codec) = codec {
            let typed = (codec.decode)(payload)?;
            Ok((typed, codec.type_id))
        } else {
            Ok((Arc::new(payload.clone()), TypeId::of::<serde_json::Value>()))
        }
    }

    async fn run_inline_effect(
        &self,
        effect: &crate::handler::Handler<D>,
        source_event: &QueuedEvent,
        typed_event: Arc<dyn Any + Send + Sync>,
        event_type_id: TypeId,
    ) -> Result<Vec<QueuedEvent>> {
        let idempotency_key = Uuid::new_v5(
            &NAMESPACE_SEESAW,
            format!("{}-{}", source_event.event_id, effect.id).as_bytes(),
        )
        .to_string();
        let ctx = Context::new(
            effect.id.clone(),
            idempotency_key,
            source_event.correlation_id,
            source_event.event_id,
            source_event.parent_id,
            self.deps.clone(),
        );
        let drained = effect
            .call_handler(typed_event, event_type_id, ctx.clone())
            .await?
            .into_iter()
            .map(|output| (output.type_id, output.value))
            .collect::<Vec<_>>();
        let emitted_count = drained.len();
        if emitted_count > self.config.max_batch_size {
            anyhow::bail!(
                "inline effect '{}' emitted {} events, exceeding max_batch_size {}",
                effect.id,
                emitted_count,
                self.config.max_batch_size
            );
        }
        if emitted_count > i32::MAX as usize {
            anyhow::bail!(
                "inline effect '{}' emitted {} events, exceeding i32 batch metadata capacity",
                effect.id,
                emitted_count
            );
        }

        let inherited_batch = if emitted_count == 1 {
            match (
                source_event.batch_id,
                source_event.batch_index,
                source_event.batch_size,
            ) {
                (Some(batch_id), Some(batch_index), Some(batch_size)) => {
                    if batch_size <= 0
                        || batch_index < 0
                        || batch_index >= batch_size
                        || batch_size as usize > self.config.max_batch_size
                    {
                        anyhow::bail!(
                            "invalid inherited batch metadata: id={} index={} size={} max_batch_size={}",
                            batch_id,
                            batch_index,
                            batch_size,
                            self.config.max_batch_size
                        );
                    }
                    Some((batch_id, batch_index, batch_size))
                }
                _ => None,
            }
        } else {
            None
        };

        let emitted_batch_id = if emitted_count > 1 {
            Some(Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!("{}-{}-batch", source_event.event_id, effect.id).as_bytes(),
            ))
        } else {
            None
        };

        let mut emitted_events = Vec::with_capacity(emitted_count);

        for (emitted_index, (type_id, event_any)) in drained.into_iter().enumerate() {
            let codec = self.effects.find_codec_by_type_id(type_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "No queue codec registered for emitted event TypeId {:?}",
                    type_id
                )
            })?;
            let payload = (codec.encode)(event_any.as_ref()).ok_or_else(|| {
                anyhow::anyhow!("Failed to serialize emitted event {}", codec.event_type)
            })?;
            let event_id = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!(
                    "{}-{}-{}-{}",
                    source_event.event_id, effect.id, codec.event_type, emitted_index
                )
                .as_bytes(),
            );
            let created_at = source_event
                .created_at
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight UTC should always be valid")
                .and_utc();
            let (batch_id, batch_index, batch_size) = if let Some(inherited) = inherited_batch {
                (Some(inherited.0), Some(inherited.1), Some(inherited.2))
            } else if emitted_count > 1 {
                (
                    emitted_batch_id,
                    Some(emitted_index as i32),
                    Some(emitted_count as i32),
                )
            } else {
                (None, None, None)
            };

            emitted_events.push(QueuedEvent {
                id: 0,
                event_id,
                parent_id: Some(source_event.event_id),
                correlation_id: source_event.correlation_id,
                event_type: codec.event_type.clone(),
                payload,
                hops: source_event.hops + 1,
                retry_count: 0,
                batch_id,
                batch_index,
                batch_size,
                created_at,
            });
        }

        Ok(emitted_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use futures::stream;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct TestDeps;

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct TypedInput {
        value: i32,
    }

    struct RetryStore {
        events: Mutex<VecDeque<QueuedEvent>>,
        nack_calls: AtomicUsize,
        ack_calls: AtomicUsize,
        dlq_calls: AtomicUsize,
    }

    impl RetryStore {
        fn with_event(event: QueuedEvent) -> Self {
            let mut events = VecDeque::new();
            events.push_back(event);
            Self {
                events: Mutex::new(events),
                nack_calls: AtomicUsize::new(0),
                ack_calls: AtomicUsize::new(0),
                dlq_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Store for RetryStore {
        async fn publish(&self, _event: QueuedEvent) -> Result<()> {
            Ok(())
        }

        async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
            Ok(self.events.lock().front().cloned())
        }

        async fn ack(&self, _id: i64) -> Result<()> {
            self.ack_calls.fetch_add(1, Ordering::SeqCst);
            self.events.lock().pop_front();
            Ok(())
        }

        async fn nack(&self, _id: i64, _retry_after_secs: u64) -> Result<()> {
            self.nack_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(event) = self.events.lock().front_mut() {
                event.retry_count += 1;
            }
            Ok(())
        }

        async fn commit_event_processing(
            &self,
            _commit: crate::EventProcessingCommit,
        ) -> Result<()> {
            Ok(())
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
            _execute_at: DateTime<Utc>,
            _timeout_seconds: i32,
            _max_attempts: i32,
            _priority: i32,
            _join_window_timeout_seconds: Option<i32>,
        ) -> Result<()> {
            Ok(())
        }

        async fn poll_next_effect(&self) -> Result<Option<crate::QueuedHandlerExecution>> {
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
            self.dlq_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn subscribe_workflow_events(
            &self,
            _correlation_id: Uuid,
        ) -> Result<Box<dyn futures::Stream<Item = crate::WorkflowEvent> + Send + Unpin>> {
            Ok(Box::new(Box::pin(stream::empty())))
        }

        async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<crate::WorkflowStatus> {
            Ok(crate::WorkflowStatus {
                correlation_id,
                pending_effects: 0,
                is_settled: true,
                last_event: None,
            })
        }
    }

    #[tokio::test]
    async fn decode_errors_respect_retry_cap_and_dlq() {
        let handler_registry = Arc::new(HandlerRegistry::new());
        handler_registry.register(
            handler::on::<TypedInput>()
                .queued()
                .id("decode_probe")
                .then(|_event: Arc<TypedInput>, _ctx: Context<TestDeps>| async move { Ok(()) }),
        );

        let bad_event = QueuedEvent {
            id: 1,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<TypedInput>().to_string(),
            payload: serde_json::json!({ "wrong_field": 42 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        };

        let store = Arc::new(RetryStore::with_event(bad_event));
        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            handler_registry,
            EventWorkerConfig {
                max_inline_retry_attempts: 2,
                ..Default::default()
            },
        );

        assert!(
            worker.process_next_event().await.is_err(),
            "first decode failure should nack for retry"
        );
        assert_eq!(store.nack_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.dlq_calls.load(Ordering::SeqCst), 0);

        assert!(
            worker.process_next_event().await.is_err(),
            "second decode failure should still nack for retry"
        );
        assert_eq!(store.nack_calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.dlq_calls.load(Ordering::SeqCst), 0);

        assert!(
            worker.process_next_event().await.is_ok(),
            "third pass should exceed retry cap and move event to DLQ"
        );
        assert_eq!(
            store.nack_calls.load(Ordering::SeqCst),
            2,
            "retry count should stop increasing once cap is reached"
        );
        assert_eq!(store.dlq_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.ack_calls.load(Ordering::SeqCst), 1);
    }
}
