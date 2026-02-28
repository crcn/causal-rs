//! In-memory Engine with built-in settle loop.
//!
//! Engine<D> publishes events to an internal MemoryStore and settles
//! the full causal tree synchronously. For durable execution, plug in
//! a `Runtime` (e.g. RestateRuntime).

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

use anyhow::Result;
use tracing::info;

use crate::aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};
use crate::handler::{Context, Handler};
use crate::handler_registry::HandlerRegistry;
use crate::runtime::{DirectRuntime, Runtime};
use crate::insight::{InsightEvent, StreamType};
use crate::job_executor::{HandlerStatus, JobExecutor};
use crate::memory_store::MemoryStore;
use crate::process::{DispatchFuture, ProcessHandle};
use crate::types::{
    EventWorkerConfig, HandlerWorkerConfig, QueuedEvent, QueuedHandlerExecution, NAMESPACE_SEESAW,
};

/// In-memory Engine with built-in settle loop.
///
/// Publishes events to an internal queue and drives them to completion
/// using `JobExecutor` + `Runtime`.
pub struct Engine<D>
where
    D: Send + Sync + 'static,
{
    store: MemoryStore,
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
    aggregators: Arc<AggregatorRegistry>,
    runtime: Arc<dyn Runtime>,
    on_insight: Option<Arc<dyn Fn(InsightEvent) + Send + Sync>>,
    insight_seq: Arc<AtomicU64>,
}

impl<D> Engine<D>
where
    D: Send + Sync + 'static,
{
    /// Create new engine with dependencies.
    pub fn new(deps: D) -> Self {
        Self {
            store: MemoryStore::new(),
            deps: Arc::new(deps),
            effects: Arc::new(HandlerRegistry::new()),
            aggregators: Arc::new(AggregatorRegistry::new()),
            runtime: Arc::new(DirectRuntime::new()),
            on_insight: None,
            insight_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Set a custom runtime (e.g. RestateRuntime for durable execution).
    pub fn with_runtime<R: Runtime + 'static>(mut self, runtime: R) -> Self {
        self.runtime = Arc::new(runtime);
        self
    }

    /// Register a handler.
    pub fn with_handler(mut self, handler: Handler<D>) -> Self {
        Arc::get_mut(&mut self.effects)
            .expect("Cannot add handler after cloning")
            .register(handler);
        self
    }

    /// Register multiple handlers.
    pub fn with_handlers<I>(mut self, handlers: I) -> Self
    where
        I: IntoIterator<Item = Handler<D>>,
    {
        let registry = Arc::get_mut(&mut self.effects).expect("Cannot add handlers after cloning");
        for handler in handlers {
            registry.register(handler);
        }
        self
    }

    /// Register an aggregator: maintains live in-memory state for aggregate `A`.
    ///
    /// `extract_id` maps the event to the aggregate ID it belongs to.
    ///
    /// ```ignore
    /// let engine = Engine::new(deps)
    ///     .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
    ///     .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id);
    /// ```
    pub fn with_aggregator<E, A, F>(mut self, extract_id: F) -> Self
    where
        E: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
        A: Aggregate + Apply<E> + serde::Serialize + serde::de::DeserializeOwned,
        F: Fn(&E) -> Uuid + Send + Sync + 'static,
    {
        // Register event codec so emitted events of this type can be serialized
        let codec = Arc::new(crate::event_codec::EventCodec {
            event_type: std::any::type_name::<E>().to_string(),
            type_id: std::any::TypeId::of::<E>(),
            decode: Arc::new(|payload| {
                let event: E = serde_json::from_value(payload.clone())?;
                Ok(Arc::new(event))
            }),
        });
        Arc::get_mut(&mut self.effects)
            .expect("Cannot add aggregator after cloning")
            .register_codec(codec);

        Arc::get_mut(&mut self.aggregators)
            .expect("Cannot add aggregator after cloning")
            .register(Aggregator::new::<E, A, F>(extract_id));
        self
    }

    /// Register multiple aggregators at once.
    pub fn with_aggregators<I>(mut self, aggregators: I) -> Self
    where
        I: IntoIterator<Item = Aggregator>,
    {
        let registry = Arc::get_mut(&mut self.aggregators)
            .expect("Cannot add aggregators after cloning");
        for aggregator in aggregators {
            registry.register(aggregator);
        }
        self
    }

    /// Set an insight callback for observability events.
    pub fn with_on_insight<F>(mut self, callback: F) -> Self
    where
        F: Fn(InsightEvent) + Send + Sync + 'static,
    {
        self.on_insight = Some(Arc::new(callback));
        self
    }

    /// Dispatch event (returns lazy future).
    ///
    /// Awaiting directly publishes the event (fire-and-forget).
    /// Chain `.settled()` to drive the full causal tree to completion:
    ///
    /// ```ignore
    /// // Fire-and-forget
    /// engine.dispatch(event).await?;
    ///
    /// // Synchronous settlement
    /// engine.dispatch(event).settled().await?;
    /// ```
    pub fn dispatch<E>(&self, event: E) -> DispatchFuture
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        let engine = self.clone();
        let engine2 = self.clone();

        let publish: crate::process::PublishFn = Box::new(move || {
            Box::pin(async move { engine.publish_event(event).await })
        });

        let settle: crate::process::SettleFn = Box::new(move || {
            Box::pin(async move { engine2.settle().await })
        });

        DispatchFuture::new(publish, settle)
    }

    /// Drive all pending events and effects to completion.
    pub async fn settle(&self) -> Result<()> {
        let executor = JobExecutor::new(
            self.deps.clone(),
            self.effects.clone(),
            self.aggregators.clone(),
            self.runtime.clone(),
        );
        let event_config = EventWorkerConfig::default();
        let handler_config = HandlerWorkerConfig::default();

        loop {
            let mut processed_any = false;

            // Drain event queue
            while let Some(event) = self.store.poll_next().await? {
                processed_any = true;

                // Apply event to live aggregator state before handlers run
                self.apply_to_aggregators(&event);

                match executor
                    .execute_event(&event, &event_config, &*self.runtime)
                    .await
                {
                    Ok(commit) => {
                        // Convert job_executor commit to memory_store commit
                        let store_commit = crate::types::EventProcessingCommit {
                            event_row_id: commit.event_row_id,
                            event_id: commit.event_id,
                            correlation_id: commit.correlation_id,
                            event_type: commit.event_type,
                            event_payload: commit.event_payload,
                            queued_effect_intents: commit.queued_effect_intents,
                            inline_effect_failures: commit
                                .inline_effect_failures
                                .into_iter()
                                .map(|f| crate::types::InlineHandlerFailure {
                                    handler_id: f.handler_id,
                                    error: f.error,
                                    reason: f.reason,
                                    attempts: f.attempts,
                                })
                                .collect(),
                            emitted_events: commit.emitted_events,
                        };
                        self.store.commit_event_processing(store_commit).await?;
                    }
                    Err(e) => {
                        // DLQ the event and continue
                        self.store
                            .dlq_effect(
                                event.event_id,
                                "__settle_event_error__".to_string(),
                                e.to_string(),
                                "settle_error".to_string(),
                                event.retry_count,
                            )
                            .await?;
                        self.store.ack(event.id).await?;
                    }
                }
            }

            // Drain all ready effects and run them in parallel
            let mut executions = Vec::new();
            while let Some(execution) = self.store.poll_next_effect().await? {
                executions.push(execution);
            }

            if !executions.is_empty() {
                processed_any = true;

                let effect_futures: Vec<_> = executions
                    .iter()
                    .map(|execution| {
                        executor.execute_handler(execution.clone(), &handler_config, &*self.runtime)
                    })
                    .collect();

                let effect_results = futures::future::join_all(effect_futures).await;

                for (exec_result, execution_clone) in effect_results.into_iter().zip(executions) {
                    let event_id = execution_clone.event_id;
                    let handler_id = execution_clone.handler_id.clone();

                    match exec_result {
                        Ok(result) => match result.status {
                            HandlerStatus::Success => {
                                self.emit_insight(self.make_insight(
                                    StreamType::EffectCompleted,
                                    execution_clone.correlation_id,
                                    Some(event_id),
                                    Some(handler_id.clone()),
                                    Some(execution_clone.event_type.clone()),
                                    Some("completed".to_string()),
                                    None,
                                    None,
                                ));
                                if result.emitted_events.is_empty() {
                                    self.store
                                        .complete_effect(event_id, handler_id, result.result)
                                        .await?;
                                } else {
                                    self.store
                                        .complete_effect_with_events(
                                            event_id,
                                            handler_id,
                                            result.result,
                                            result.emitted_events,
                                            execution_clone.correlation_id,
                                            execution_clone.hops,
                                        )
                                        .await?;
                                }
                            }
                            HandlerStatus::Failed { error, attempts } => {
                                self.emit_insight(self.make_insight(
                                    StreamType::EffectFailed,
                                    execution_clone.correlation_id,
                                    Some(event_id),
                                    Some(handler_id.clone()),
                                    Some(execution_clone.event_type.clone()),
                                    Some("failed".to_string()),
                                    Some(error.clone()),
                                    None,
                                ));
                                if result.emitted_events.is_empty() {
                                    self.store
                                        .dlq_effect(
                                            event_id,
                                            handler_id,
                                            error,
                                            "failed".to_string(),
                                            attempts,
                                        )
                                        .await?;
                                } else {
                                    self.store
                                        .dlq_effect_with_events(
                                            event_id,
                                            handler_id,
                                            error,
                                            "failed".to_string(),
                                            attempts,
                                            result.emitted_events,
                                            execution_clone.correlation_id,
                                            execution_clone.hops,
                                        )
                                        .await?;
                                }
                            }
                            HandlerStatus::Retry { error, attempts } => {
                                self.store
                                    .fail_effect(
                                        event_id,
                                        handler_id,
                                        error,
                                        attempts,
                                        execution_clone,
                                    )
                                    .await?;
                            }
                            HandlerStatus::Timeout => {
                                self.store
                                    .dlq_effect(
                                        event_id,
                                        handler_id,
                                        "timeout".to_string(),
                                        "timeout".to_string(),
                                        0,
                                    )
                                    .await?;
                            }
                            HandlerStatus::JoinWaiting => {
                                // Wire join/accumulate coordination
                                if let Some(join_claim) = result.join_claim {
                                    self.handle_join_waiting(
                                        &executor,
                                        event_id,
                                        &handler_id,
                                        &execution_clone,
                                        join_claim.batch_id,
                                    )
                                    .await?;
                                } else {
                                    self.store
                                        .complete_effect(
                                            event_id,
                                            handler_id,
                                            serde_json::json!({ "status": "join_waiting" }),
                                        )
                                        .await?;
                                }
                            }
                        },
                        Err(e) => {
                            self.store
                                .dlq_effect(
                                    event_id,
                                    handler_id,
                                    e.to_string(),
                                    "settle_handler_error".to_string(),
                                    0,
                                )
                                .await?;
                        }
                    }
                }
            }

            if !processed_any {
                break; // Settled!
            }
        }

        Ok(())
    }

    // --- Internal ---

    async fn publish_event<E>(&self, event: E) -> Result<ProcessHandle>
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        let event_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();
        let event_type = std::any::type_name::<E>().to_string();
        let payload = serde_json::to_value(&event).expect("Event must be serializable");

        info!(
            "Publishing event: type={}, correlation_id={}",
            event_type, correlation_id
        );

        let queued = QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            correlation_id,
            event_type: event_type.clone(),
            payload: payload.clone(),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: chrono::Utc::now(),
        };

        self.store.publish(queued).await?;

        self.emit_insight(self.make_insight(
            StreamType::EventDispatched,
            correlation_id,
            Some(event_id),
            None,
            Some(event_type),
            None,
            None,
            Some(payload),
        ));

        Ok(ProcessHandle {
            correlation_id,
            event_id,
        })
    }

    /// Apply event to aggregator state via the runtime.
    fn apply_to_aggregators(&self, event: &QueuedEvent) {
        self.aggregators
            .apply_event(&event.event_type, &event.payload, &*self.runtime);
    }

    fn emit_insight(&self, event: InsightEvent) {
        if let Some(ref cb) = self.on_insight {
            cb(event);
        }
    }

    fn make_insight(
        &self,
        stream_type: StreamType,
        correlation_id: Uuid,
        event_id: Option<Uuid>,
        handler_id: Option<String>,
        event_type: Option<String>,
        status: Option<String>,
        error: Option<String>,
        payload: Option<serde_json::Value>,
    ) -> InsightEvent {
        InsightEvent {
            seq: self.insight_seq.fetch_add(1, Ordering::SeqCst) as i64,
            stream_type,
            correlation_id,
            event_id,
            effect_event_id: event_id,
            handler_id,
            event_type,
            status,
            error,
            payload,
            created_at: chrono::Utc::now(),
        }
    }

    async fn handle_join_waiting(
        &self,
        executor: &JobExecutor<D>,
        event_id: Uuid,
        handler_id: &str,
        execution: &QueuedHandlerExecution,
        batch_id: Uuid,
    ) -> Result<()> {
        let batch_index = execution.batch_index.unwrap_or(0);
        let batch_size = execution.batch_size.unwrap_or(1);
        let join_window_timeout = execution.join_window_timeout_seconds;

        let entries = self
            .store
            .join_same_batch_append_and_maybe_claim(
                handler_id.to_string(),
                execution.correlation_id,
                event_id,
                execution.event_type.clone(),
                execution.event_payload.clone(),
                execution.execute_at,
                batch_id,
                batch_index,
                batch_size,
                join_window_timeout,
            )
            .await?;

        match entries {
            Some(entries) => {
                // All items arrived — decode and call join batch handler
                let effect = executor.effects().find_by_id(handler_id);
                if let Some(effect) = effect {
                    let mut typed_values: Vec<Arc<dyn Any + Send + Sync>> =
                        Vec::with_capacity(entries.len());
                    for entry in &entries {
                        let codec = executor
                            .effects()
                            .find_codec_by_event_type(&entry.event_type);
                        if let Some(codec) = codec {
                            let decoded = (codec.decode)(&entry.payload)?;
                            typed_values.push(decoded);
                        } else {
                            typed_values.push(Arc::new(entry.payload.clone()));
                        }
                    }

                    let idempotency_key = Uuid::new_v5(
                        &NAMESPACE_SEESAW,
                        format!("{}-{}-join", batch_id, handler_id).as_bytes(),
                    )
                    .to_string();

                    let ctx = Context::new(
                        handler_id.to_string(),
                        idempotency_key,
                        execution.correlation_id,
                        event_id,
                        execution.parent_event_id,
                        self.deps.clone(),
                    )
                    .with_aggregator_registry(self.aggregators.clone())
                    .with_runtime(self.runtime.clone());

                    match effect.call_join_batch_handler(typed_values, ctx).await {
                        Ok(outputs) => {
                            // Serialize emitted events
                            let handler_config = HandlerWorkerConfig::default();
                            let emitted_events = executor.serialize_emitted_events(
                                outputs,
                                execution,
                                handler_config.max_batch_size,
                            )?;

                            if emitted_events.is_empty() {
                                self.store
                                    .complete_effect(
                                        event_id,
                                        handler_id.to_string(),
                                        serde_json::json!({ "status": "join_completed" }),
                                    )
                                    .await?;
                            } else {
                                self.store
                                    .complete_effect_with_events(
                                        event_id,
                                        handler_id.to_string(),
                                        serde_json::json!({ "status": "join_completed" }),
                                        emitted_events,
                                        execution.correlation_id,
                                        execution.hops,
                                    )
                                    .await?;
                            }

                            self.store
                                .join_same_batch_complete(
                                    handler_id.to_string(),
                                    execution.correlation_id,
                                    batch_id,
                                )
                                .await?;
                        }
                        Err(e) => {
                            self.store
                                .join_same_batch_release(
                                    handler_id.to_string(),
                                    execution.correlation_id,
                                    batch_id,
                                    e.to_string(),
                                )
                                .await?;
                            self.store
                                .dlq_effect(
                                    event_id,
                                    handler_id.to_string(),
                                    e.to_string(),
                                    "join_handler_error".to_string(),
                                    0,
                                )
                                .await?;
                        }
                    }
                } else {
                    self.store
                        .complete_effect(
                            event_id,
                            handler_id.to_string(),
                            serde_json::json!({ "status": "join_handler_not_found" }),
                        )
                        .await?;
                }
            }
            None => {
                // Still waiting for more items — mark complete (no-op)
                self.store
                    .complete_effect(
                        event_id,
                        handler_id.to_string(),
                        serde_json::json!({ "status": "join_waiting" }),
                    )
                    .await?;
            }
        }

        Ok(())
    }
}

impl<D> Clone for Engine<D>
where
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            deps: self.deps.clone(),
            effects: self.effects.clone(),
            aggregators: self.aggregators.clone(),
            runtime: self.runtime.clone(),
            on_insight: self.on_insight.clone(),
            insight_seq: self.insight_seq.clone(),
        }
    }
}
