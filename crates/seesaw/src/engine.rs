//! In-memory Engine with built-in settle loop.
//!
//! Engine<D> publishes events to an internal MemoryStore and settles
//! the full causal tree synchronously. For durable execution, plug in
//! a `Runtime` (e.g. RestateRuntime).

use std::any::Any;
use std::sync::Arc;
use uuid::Uuid;

use anyhow::Result;
use tracing::info;

use crate::aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};
use crate::event_store::{
    event_type_short_name, EventStore, NewEvent, SnapshotStore,
};
use crate::handler::{Context, Handler};
use crate::handler::context::{DirectRunner, SideEffectRunner};
use crate::handler_registry::HandlerRegistry;
use crate::runtime::{DirectRuntime, Runtime};
use crate::job_executor::{HandlerStatus, JobExecutor};
use crate::memory_store::MemoryStore;
use crate::process::{EmitFuture, ProcessHandle, SettleWithRuntimeFn};
use crate::types::{
    EventWorkerConfig, HandlerWorkerConfig, QueuedEvent, QueuedHandlerExecution, NAMESPACE_SEESAW,
};
use crate::upcaster::{Upcaster, UpcasterRegistry};

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
    upcasters: Arc<UpcasterRegistry>,
    runtime: Arc<dyn Runtime>,
    side_effect_runner: Arc<dyn SideEffectRunner>,
    event_store: Option<Arc<dyn EventStore>>,
    snapshot_store: Option<Arc<dyn SnapshotStore>>,
    snapshot_every: Option<u64>,
    event_metadata: serde_json::Map<String, serde_json::Value>,
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
            upcasters: Arc::new(UpcasterRegistry::new()),
            runtime: Arc::new(DirectRuntime::new()),
            side_effect_runner: Arc::new(DirectRunner),
            event_store: None,
            snapshot_store: None,
            snapshot_every: None,
            event_metadata: serde_json::Map::new(),
        }
    }

    /// Access the shared dependencies.
    pub fn deps(&self) -> &Arc<D> {
        &self.deps
    }

    /// Read aggregate state by ID. Returns `Arc::new(A::default())` if no state exists.
    pub fn aggregate<A: Aggregate + 'static>(&self, id: Uuid) -> Arc<A> {
        self.aggregators.get_transition_arc::<A>(id).1
    }

    /// Read singleton aggregate state. Returns `Arc::new(A::default())` if no state exists.
    pub fn singleton<A: Aggregate + 'static>(&self) -> Arc<A> {
        self.aggregators.get_singleton_arc::<A>().1
    }

    /// Invalidate cached aggregate state, forcing re-hydration from the EventStore.
    ///
    /// Use after ingesting foreign events (e.g. from another node) so the
    /// next settle loop rebuilds the aggregate from the persistent log.
    pub fn invalidate_aggregate<A: Aggregate>(&self, id: Uuid) {
        let key = format!("{}:{}", A::aggregate_type(), id);
        self.aggregators.remove_state(&key);
    }

    /// Set a custom runtime (e.g. RestateRuntime for durable execution).
    pub fn with_runtime<R: Runtime + 'static>(mut self, runtime: R) -> Self {
        self.runtime = Arc::new(runtime);
        self
    }

    /// Set a custom side-effect runner for journaled `ctx.run()` execution.
    ///
    /// Durable runtimes (e.g. Restate) provide runners that journal side-effect
    /// results. The default `DirectRunner` executes closures inline with no journaling.
    pub fn with_side_effect_runner<R: SideEffectRunner + 'static>(mut self, runner: R) -> Self {
        self.side_effect_runner = Arc::new(runner);
        self
    }

    /// Set a persistent event store for auto-persist and cold-start hydration.
    ///
    /// When set, events matching registered aggregators are automatically
    /// persisted, and aggregates are hydrated from the store on cold access.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore>) -> Self {
        self.event_store = Some(store);
        self
    }

    /// Set a snapshot store for hydration acceleration.
    ///
    /// Optional optimization — without it, cold-start hydration replays
    /// all events from the EventStore.
    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// Enable auto-checkpoint snapshots every N events.
    ///
    /// Requires a snapshot store to be set. When both are configured,
    /// snapshots are saved automatically during the settle loop.
    pub fn snapshot_every(mut self, events: u64) -> Self {
        self.snapshot_every = Some(events);
        self
    }

    /// Set metadata to stamp on every persisted event.
    ///
    /// Metadata travels with the event through the EventStore, letting
    /// adapters pull application-level context (e.g. `run_id`, `schema_v`,
    /// `actor`) without holding state themselves.
    ///
    /// ```ignore
    /// let engine = Engine::new(deps)
    ///     .with_event_store(event_store)
    ///     .with_event_metadata(serde_json::json!({
    ///         "run_id": "scrape-abc123",
    ///         "schema_v": 1
    ///     }));
    /// ```
    pub fn with_event_metadata(mut self, metadata: serde_json::Value) -> Self {
        if let serde_json::Value::Object(map) = metadata {
            self.event_metadata = map;
        } else {
            panic!("with_event_metadata expects a JSON object");
        }
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

    /// Register an upcaster for event type `E`.
    ///
    /// Upcasters transform old event payloads to the current schema during decode.
    /// Chain multiple upcasters to evolve through versions: v1 → v2 → v3 → current.
    ///
    /// ```ignore
    /// let engine = Engine::new(deps)
    ///     .with_upcaster::<OrderPlaced>(1, |mut v: serde_json::Value| {
    ///         v["currency"] = serde_json::json!("USD");
    ///         Ok(v)
    ///     });
    /// ```
    pub fn with_upcaster<E, F>(mut self, from_version: u32, transform: F) -> Self
    where
        E: 'static,
        F: Fn(serde_json::Value) -> anyhow::Result<serde_json::Value> + Send + Sync + 'static,
    {
        let short_name = event_type_short_name(std::any::type_name::<E>()).to_string();
        Arc::get_mut(&mut self.upcasters)
            .expect("Cannot add upcaster after cloning")
            .register(Upcaster {
                event_type: short_name,
                from_version,
                transform: Arc::new(transform),
            });
        self
    }

    /// Emit an event into the engine (returns lazy future).
    ///
    /// Awaiting directly publishes the event (fire-and-forget).
    /// Chain `.settled()` to drive the full causal tree to completion:
    ///
    /// ```ignore
    /// // Fire-and-forget
    /// engine.emit(event).await?;
    ///
    /// // Synchronous settlement
    /// engine.emit(event).settled().await?;
    /// ```
    pub fn emit<E>(&self, event: E) -> EmitFuture
    where
        E: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        let engine = self.clone();
        let engine2 = self.clone();
        let engine3 = self.clone();

        let publish: crate::process::PublishFn = Box::new(move || {
            Box::pin(async move { engine.publish_event(event).await })
        });

        let settle: crate::process::SettleFn = Box::new(move || {
            Box::pin(async move { engine2.settle().await })
        });

        let settle_with: SettleWithRuntimeFn = Box::new(move |runtime| {
            Box::pin(async move { engine3.settle_with(runtime).await })
        });

        EmitFuture::new(publish, settle, settle_with)
    }

    /// Emit a type-erased `EventOutput` directly.
    ///
    /// Use when you have heterogeneous events from an `Events` bag
    /// and need to emit them without downcasting.
    ///
    /// ```ignore
    /// for output in events.into_outputs() {
    ///     engine.emit_output(output).settled().await?;
    /// }
    /// ```
    pub fn emit_output(&self, output: crate::handler::EventOutput) -> EmitFuture {
        let engine = self.clone();
        let engine2 = self.clone();
        let engine3 = self.clone();

        let publish: crate::process::PublishFn = Box::new(move || {
            Box::pin(async move { engine.publish_output(output).await })
        });

        let settle: crate::process::SettleFn = Box::new(move || {
            Box::pin(async move { engine2.settle().await })
        });

        let settle_with: SettleWithRuntimeFn = Box::new(move |runtime| {
            Box::pin(async move { engine3.settle_with(runtime).await })
        });

        EmitFuture::new(publish, settle, settle_with)
    }

    /// Deprecated: use `emit()` instead.
    #[deprecated(note = "renamed to emit()")]
    pub fn dispatch<E>(&self, event: E) -> EmitFuture
    where
        E: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        self.emit(event)
    }

    /// Drive all pending events and effects to completion.
    pub async fn settle(&self) -> Result<()> {
        self.settle_inner(&*self.runtime).await
    }

    /// Drive all pending events and effects to completion using a borrowed runtime.
    ///
    /// This allows using borrowed runtimes (e.g. Restate's `WorkflowContext<'ctx>`)
    /// that can't be stored in `Arc<dyn Runtime>`.
    pub async fn settle_with(&self, runtime: &dyn Runtime) -> Result<()> {
        self.settle_inner(runtime).await
    }

    async fn settle_inner(&self, runtime: &dyn Runtime) -> Result<()> {
        let executor = JobExecutor::new(
            self.deps.clone(),
            self.effects.clone(),
            self.aggregators.clone(),
            self.upcasters.clone(),
            self.side_effect_runner.clone(),
        );
        let event_config = EventWorkerConfig::default();
        let handler_config = HandlerWorkerConfig::default();

        loop {
            let mut processed_any = false;

            // Drain event queue
            while let Some(event) = self.store.poll_next().await? {
                processed_any = true;

                // Persist every event + hydrate cold aggregates before state mutation.
                // The settle loop processes events sequentially (single-writer),
                // so multiple events targeting the same aggregate in one batch
                // are safe — each sees the version left by the previous one.
                if self.event_store.is_some() {
                    self.persist_and_hydrate(&event).await?;
                }

                // Apply event to live aggregator state before handlers run
                self.apply_to_aggregators(&event);

                // Auto-checkpoint snapshots if configured
                if let (Some(snapshot_store), Some(threshold)) = (&self.snapshot_store, self.snapshot_every) {
                    self.maybe_auto_snapshot(&event, snapshot_store.as_ref(), threshold).await?;
                }

                match executor
                    .execute_event(&event, &event_config, runtime)
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
                        executor.execute_handler(execution.clone(), &handler_config, runtime)
                    })
                    .collect();

                let effect_results = futures::future::join_all(effect_futures).await;

                for (exec_result, execution_clone) in effect_results.into_iter().zip(executions) {
                    let event_id = execution_clone.event_id;
                    let handler_id = execution_clone.handler_id.clone();

                    match exec_result {
                        Ok(result) => match result.status {
                            HandlerStatus::Success => {
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
                                // Apply exponential backoff if configured
                                let mut retried = execution_clone;
                                if let Some(effect) = executor.effects().find_by_id(&handler_id) {
                                    if let Some(base) = effect.backoff {
                                        let multiplier = 2u64.saturating_pow(attempts.max(1) as u32 - 1);
                                        let delay = base.saturating_mul(multiplier as u32);
                                        retried.execute_at = chrono::Utc::now()
                                            + chrono::Duration::from_std(delay)
                                                .unwrap_or(chrono::Duration::seconds(60));
                                    }
                                }
                                self.store
                                    .fail_effect(
                                        event_id,
                                        handler_id,
                                        error,
                                        attempts,
                                        retried,
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
                // Check if there are future-dated effects (from backoff or delay).
                // If so, sleep until the earliest one becomes ready instead of exiting.
                if let Some(earliest) = self.store.earliest_pending_effect_at() {
                    let now = chrono::Utc::now();
                    if earliest > now {
                        let wait = (earliest - now).to_std().unwrap_or_default();
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
                break; // Settled!
            }
        }

        Ok(())
    }

    // --- Internal ---

    /// Hydrate cold aggregates, then persist every event to the global log.
    ///
    /// 1. For each matching aggregator, hydrate cold aggregates from EventStore
    /// 2. Always append the event (with aggregate metadata if aggregators match)
    async fn persist_and_hydrate(&self, event: &QueuedEvent) -> Result<()> {
        let event_store = self.event_store.as_ref().unwrap();
        let short_name = event_type_short_name(&event.event_type);

        // Find matching aggregators to set aggregate metadata
        let matching = self.aggregators.find_by_event_type(&event.event_type);

        // Determine aggregate metadata from the first matching aggregator
        let (aggregate_type, aggregate_id) = matching.iter().find_map(|agg| {
            agg.extract_id_from_json(&event.payload)
                .map(|id| (agg.aggregate_type.clone(), id))
        }).map(|(t, id)| (Some(t), Some(id))).unwrap_or((None, None));

        // Step 1: Hydrate cold aggregates BEFORE appending (so we don't load
        // the event we're about to append)
        for agg in &matching {
            let agg_id = match agg.extract_id_from_json(&event.payload) {
                Some(id) => id,
                None => continue,
            };

            let key = format!("{}:{}", agg.aggregate_type, agg_id);

            if !self.aggregators.has_state(&key) {
                self.hydrate_aggregate(
                    event_store.as_ref(),
                    &agg.aggregate_type,
                    agg_id,
                    &key,
                )
                .await?;
            }
        }

        // Step 2: Always append to global log
        event_store
            .append(NewEvent {
                event_id: event.event_id,
                parent_id: event.parent_id,
                correlation_id: event.correlation_id,
                event_type: short_name.to_string(),
                payload: event.payload.clone(),
                created_at: event.created_at,
                aggregate_type,
                aggregate_id,
                metadata: self.event_metadata.clone(),
            })
            .await?;

        Ok(())
    }

    /// Hydrate a single aggregate from EventStore (with optional snapshot acceleration).
    async fn hydrate_aggregate(
        &self,
        event_store: &dyn EventStore,
        aggregate_type: &str,
        aggregate_id: Uuid,
        key: &str,
    ) -> Result<()> {
        // Try snapshot store first
        if let Some(snapshot_store) = &self.snapshot_store {
            if let Some(snapshot) = snapshot_store
                .load_snapshot(aggregate_type, aggregate_id)
                .await?
            {
                let agg = self.aggregators.find_first_by_aggregate_type(aggregate_type);
                if let Some(agg) = agg {
                    let mut state = agg.deserialize_state(snapshot.state)?;

                    // Load remaining events after snapshot position
                    let remaining = event_store
                        .load_stream_from(aggregate_type, aggregate_id, snapshot.version)
                        .await?;

                    if !remaining.is_empty() {
                        let event_pairs: Vec<(&str, &serde_json::Value)> = remaining
                            .iter()
                            .map(|e| (e.event_type.as_str(), &e.payload))
                            .collect();

                        self.aggregators.replay_events_onto(
                            aggregate_type,
                            state.as_mut(),
                            &event_pairs,
                            &self.upcasters,
                        )?;
                    }

                    let final_version = snapshot.version + remaining.len() as u64;
                    self.aggregators
                        .set_state(key, Arc::from(state), final_version, snapshot.version);
                    return Ok(());
                }
            }
        }

        // No snapshot — full replay
        let events = event_store.load_stream(aggregate_type, aggregate_id).await?;
        if events.is_empty() {
            return Ok(());
        }

        let event_pairs: Vec<(&str, &serde_json::Value)> = events
            .iter()
            .map(|e| (e.event_type.as_str(), &e.payload))
            .collect();

        let last_version = events.last().unwrap().version.unwrap_or(events.len() as u64);

        if let Some(state) = self.aggregators.replay_events(
            aggregate_type,
            &event_pairs,
            &self.upcasters,
        )? {
            self.aggregators
                .set_state(key, Arc::from(state), last_version, 0);
        }

        Ok(())
    }

    async fn publish_event<E>(&self, event: E) -> Result<ProcessHandle>
    where
        E: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        // Auto-register codec so the event can be decoded in the settle loop
        let codec = Arc::new(crate::event_codec::EventCodec {
            event_type: std::any::type_name::<E>().to_string(),
            type_id: std::any::TypeId::of::<E>(),
            decode: Arc::new(|payload| {
                let event: E = serde_json::from_value(payload.clone())?;
                Ok(Arc::new(event))
            }),
        });
        self.effects.register_codec(codec);

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

        Ok(ProcessHandle {
            correlation_id,
            event_id,
        })
    }

    async fn publish_output(&self, output: crate::handler::EventOutput) -> Result<ProcessHandle> {
        self.effects.register_codec(output.codec.clone());

        let payload = (output.encode)(output.value.as_ref()).ok_or_else(|| {
            anyhow::anyhow!("Failed to serialize event {}", output.event_type)
        })?;

        let event_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();

        info!(
            "Publishing event output: type={}, correlation_id={}",
            output.event_type, correlation_id
        );

        let queued = QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            correlation_id,
            event_type: output.event_type,
            payload,
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: chrono::Utc::now(),
        };

        self.store.publish(queued).await?;

        Ok(ProcessHandle {
            correlation_id,
            event_id,
        })
    }

    /// Auto-snapshot aggregates if the event count since last snapshot exceeds the threshold.
    async fn maybe_auto_snapshot(
        &self,
        event: &QueuedEvent,
        snapshot_store: &dyn SnapshotStore,
        threshold: u64,
    ) -> Result<()> {
        let matching = self.aggregators.find_by_event_type(&event.event_type);

        for agg in matching {
            let aggregate_id = match agg.extract_id_from_json(&event.payload) {
                Some(id) => id,
                None => continue,
            };

            let key = format!("{}:{}", agg.aggregate_type, aggregate_id);
            let version = self.aggregators.get_version(&key);
            let snapshot_at = self.aggregators.get_snapshot_at_version(&key);

            if version - snapshot_at >= threshold {
                let state_ref = match self.aggregators.get_state(&key) {
                    Some(s) => s,
                    None => continue,
                };

                let state_json = agg.serialize_state(state_ref.as_ref())?;

                snapshot_store
                    .save_snapshot(crate::event_store::Snapshot {
                        aggregate_type: agg.aggregate_type.clone(),
                        aggregate_id,
                        version,
                        state: state_json,
                        created_at: chrono::Utc::now(),
                    })
                    .await?;

                self.aggregators.update_snapshot_at_version(&key, version);
            }
        }

        Ok(())
    }

    /// Apply event to aggregator state.
    fn apply_to_aggregators(&self, event: &QueuedEvent) {
        self.aggregators
            .apply_event(&event.event_type, &event.payload);
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
                    .with_aggregator_registry(self.aggregators.clone());

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
            upcasters: self.upcasters.clone(),
            runtime: self.runtime.clone(),
            side_effect_runner: self.side_effect_runner.clone(),
            event_store: self.event_store.clone(),
            snapshot_store: self.snapshot_store.clone(),
            snapshot_every: self.snapshot_every,
            event_metadata: self.event_metadata.clone(),
        }
    }
}
