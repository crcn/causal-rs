//! Store-agnostic Engine with built-in settle loop.
//!
//! Engine<D> publishes events to a pluggable [`Store`](crate::store::Store)
//! backend and settles the full causal tree synchronously.

use std::any::Any;
use std::sync::Arc;
use uuid::Uuid;

use anyhow::Result;
use tracing::info;

use crate::aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};
use crate::event_store::event_type_short_name;
use crate::handler::{Context, GlobalDlqMapper, Handler};
use crate::handler_registry::HandlerRegistry;
use crate::job_executor::{HandlerStatus, JobExecutor};
use crate::memory_store::MemoryStore;
use crate::process::{EmitFuture, ProcessHandle};
use crate::store::Store;
use crate::types::{
    EffectCompletion, EffectDlq, EmittedEvent, EventWorkerConfig, HandlerWorkerConfig,
    JoinAppendParams, NewEvent, QueuedEvent, QueuedHandlerExecution, Snapshot, NAMESPACE_SEESAW,
};
use crate::upcaster::{Upcaster, UpcasterRegistry};

/// Store-agnostic Engine with built-in settle loop.
///
/// Publishes events to a pluggable [`Store`] backend (defaults to an
/// in-memory store) and drives them to completion using `JobExecutor`.
///
/// Supply a custom store via [`with_store`](Engine::with_store) for
/// durability, crash recovery, or distributed workers.
pub struct Engine<D>
where
    D: Send + Sync + 'static,
{
    store: Arc<dyn Store>,
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
    aggregators: Arc<AggregatorRegistry>,
    upcasters: Arc<UpcasterRegistry>,
    snapshot_every: Option<u64>,
    event_metadata: serde_json::Map<String, serde_json::Value>,
    global_dlq_mapper: Option<GlobalDlqMapper>,
}

impl<D> Engine<D>
where
    D: Send + Sync + 'static,
{
    /// Create new engine with dependencies.
    ///
    /// Defaults to an in-memory store. Use [`with_store`](Engine::with_store)
    /// to supply a Postgres or other durable backend.
    pub fn new(deps: D) -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            deps: Arc::new(deps),
            effects: Arc::new(HandlerRegistry::new()),
            aggregators: Arc::new(AggregatorRegistry::new()),
            upcasters: Arc::new(UpcasterRegistry::new()),
            snapshot_every: None,
            event_metadata: serde_json::Map::new(),
            global_dlq_mapper: None,
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

    /// Invalidate cached aggregate state, forcing re-hydration from the Store.
    ///
    /// Use after ingesting foreign events (e.g. from another node) so the
    /// next settle loop rebuilds the aggregate from the persistent log.
    pub fn invalidate_aggregate<A: Aggregate>(&self, id: Uuid) {
        let key = format!("{}:{}", A::aggregate_type(), id);
        self.aggregators.remove_state(&key);
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
    /// Metadata travels with the event through the Store, letting
    /// adapters pull application-level context (e.g. `run_id`, `schema_v`,
    /// `actor`) without holding state themselves.
    ///
    /// ```ignore
    /// let engine = Engine::new(deps)
    ///     .with_store(my_store)
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

    /// Supply a custom store backend.
    ///
    /// Replaces the default in-memory store with the provided implementation.
    ///
    /// ```ignore
    /// let engine = Engine::new(deps)
    ///     .with_store(Arc::new(PostgresStore::new(pool)));
    /// ```
    pub fn with_store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = store;
        self
    }

    /// Register a global DLQ callback that fires when any handler exhausts retries.
    ///
    /// The callback receives [`DlqTerminalInfo`] with handler and event metadata,
    /// and returns a serializable event that gets dispatched into the causal chain.
    ///
    /// Per-handler `on_failure` takes precedence when present.
    ///
    /// ```ignore
    /// let engine = Engine::new(deps)
    ///     .on_dlq(|info: DlqTerminalInfo| HandlerFailed {
    ///         handler_id: info.handler_id.clone(),
    ///         error: info.error.clone(),
    ///     });
    /// ```
    pub fn on_dlq<E, F>(mut self, mapper: F) -> Self
    where
        E: serde::Serialize + Send + Sync + 'static,
        F: Fn(crate::handler::DlqTerminalInfo) -> E + Send + Sync + 'static,
    {
        let event_type = std::any::type_name::<E>().to_string();
        self.global_dlq_mapper = Some(Arc::new(move |info| {
            let event = mapper(info);
            Ok(crate::EmittedEvent {
                event_type: event_type.clone(),
                payload: serde_json::to_value(&event)?,
                batch_id: None,
                batch_index: None,
                batch_size: None,
                handler_id: None,
            })
        }));
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

        let publish: crate::process::PublishFn = Box::new(move |correlation_id| {
            Box::pin(async move { engine.publish_event(event, correlation_id).await })
        });

        let settle: crate::process::SettleFn = Box::new(move || {
            Box::pin(async move { engine2.settle().await })
        });

        EmitFuture::new(publish, settle)
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

        let publish: crate::process::PublishFn = Box::new(move |correlation_id| {
            Box::pin(async move { engine.publish_output(output, correlation_id).await })
        });

        let settle: crate::process::SettleFn = Box::new(move || {
            Box::pin(async move { engine2.settle().await })
        });

        EmitFuture::new(publish, settle)
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
        let executor = JobExecutor::new(
            self.deps.clone(),
            self.effects.clone(),
            self.aggregators.clone(),
            self.upcasters.clone(),
            self.global_dlq_mapper.clone(),
        );
        let event_config = EventWorkerConfig::default();
        let handler_config = HandlerWorkerConfig::default();

        loop {
            let mut processed_any = false;

            // Drain event queue
            while let Some(event) = self.store.poll_next().await? {
                processed_any = true;

                // Persist every event + hydrate cold aggregates before state mutation.
                // Default Store no-ops make this safe for MemoryStore.
                self.persist_and_hydrate(&event).await?;

                // Apply event to live aggregator state before handlers run
                self.apply_to_aggregators(&event);

                // Auto-checkpoint snapshots if configured
                if let Some(threshold) = self.snapshot_every {
                    self.maybe_auto_snapshot(&event, threshold).await?;
                }

                match executor.execute_event(&event, &event_config).await {
                    Ok(commit) => {
                        self.store.commit_event_processing(commit).await?;
                    }
                    Err(e) => {
                        // Reject the event (DLQ + ack atomically)
                        self.store
                            .reject_event(
                                event.id,
                                event.event_id,
                                e.to_string(),
                                "settle_error".to_string(),
                            )
                            .await?;
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
                        executor.execute_handler(execution.clone(), &handler_config)
                    })
                    .collect();

                let effect_results = futures::future::join_all(effect_futures).await;

                for (exec_result, execution_clone) in
                    effect_results.into_iter().zip(executions)
                {
                    let event_id = execution_clone.event_id;
                    let handler_id = execution_clone.handler_id.clone();

                    match exec_result {
                        Ok(result) => match result.status {
                            HandlerStatus::Success => {
                                let events_to_publish = Self::build_queued_events(
                                    result.emitted_events,
                                    event_id,
                                    &handler_id,
                                    execution_clone.correlation_id,
                                    execution_clone.hops,
                                    "",
                                );
                                self.store
                                    .complete_effect(EffectCompletion {
                                        event_id,
                                        handler_id,
                                        result: result.result,
                                        events_to_publish,
                                    })
                                    .await?;
                            }
                            HandlerStatus::Failed { error, attempts } => {
                                let events_to_publish = Self::build_queued_events(
                                    result.emitted_events,
                                    event_id,
                                    &handler_id,
                                    execution_clone.correlation_id,
                                    execution_clone.hops,
                                    "-dlq",
                                );
                                self.store
                                    .dlq_effect(EffectDlq {
                                        event_id,
                                        handler_id,
                                        error,
                                        reason: "failed".to_string(),
                                        attempts,
                                        events_to_publish,
                                    })
                                    .await?;
                            }
                            HandlerStatus::Retry { error, attempts } => {
                                // Compute backoff schedule in Engine
                                let mut next_execute_at = chrono::Utc::now();
                                if let Some(effect) =
                                    executor.effects().find_by_id(&handler_id)
                                {
                                    if let Some(base) = effect.backoff {
                                        let multiplier =
                                            2u64.saturating_pow(attempts.max(1) as u32 - 1);
                                        let delay = base.saturating_mul(multiplier as u32);
                                        next_execute_at = chrono::Utc::now()
                                            + chrono::Duration::from_std(delay)
                                                .unwrap_or(chrono::Duration::seconds(60));
                                    }
                                }
                                self.store
                                    .fail_effect(
                                        event_id,
                                        handler_id,
                                        error,
                                        attempts + 1,
                                        next_execute_at,
                                    )
                                    .await?;
                            }
                            HandlerStatus::Timeout => {
                                let dlq_events = self
                                    .build_global_dlq_event(
                                        &execution_clone,
                                        "Handler execution timed out".to_string(),
                                        "timeout",
                                    )
                                    .unwrap_or_default();
                                let events_to_publish = Self::build_queued_events(
                                    dlq_events,
                                    event_id,
                                    &handler_id,
                                    execution_clone.correlation_id,
                                    execution_clone.hops,
                                    "-dlq",
                                );
                                self.store
                                    .dlq_effect(EffectDlq {
                                        event_id,
                                        handler_id,
                                        error: "timeout".to_string(),
                                        reason: "timeout".to_string(),
                                        attempts: 0,
                                        events_to_publish,
                                    })
                                    .await?;
                            }
                            HandlerStatus::JoinWaiting => {
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
                                        .complete_effect(EffectCompletion {
                                            event_id,
                                            handler_id,
                                            result: serde_json::json!({ "status": "join_waiting" }),
                                            events_to_publish: Vec::new(),
                                        })
                                        .await?;
                                }
                            }
                        },
                        Err(e) => {
                            let error_str = e.to_string();
                            let dlq_events = self
                                .build_global_dlq_event(
                                    &execution_clone,
                                    error_str.clone(),
                                    "settle_handler_error",
                                )
                                .unwrap_or_default();
                            let events_to_publish = Self::build_queued_events(
                                dlq_events,
                                event_id,
                                &handler_id,
                                execution_clone.correlation_id,
                                execution_clone.hops,
                                "-dlq",
                            );
                            self.store
                                .dlq_effect(EffectDlq {
                                    event_id,
                                    handler_id,
                                    error: error_str,
                                    reason: "settle_handler_error".to_string(),
                                    attempts: 0,
                                    events_to_publish,
                                })
                                .await?;
                        }
                    }
                }
            }

            // Expire timed-out join windows and DLQ each one
            let expired_windows = self.store.expire_join_windows(chrono::Utc::now()).await?;
            if !expired_windows.is_empty() {
                processed_any = true;
                for window in expired_windows {
                    tracing::warn!(
                        "Join window expired: handler={}, correlation={}, batch={}, events={}",
                        window.join_handler_id,
                        window.correlation_id,
                        window.batch_id,
                        window.source_event_ids.len()
                    );

                    // DLQ each source event in the expired window
                    for source_event_id in &window.source_event_ids {
                        self.store
                            .dlq_effect(EffectDlq {
                                event_id: *source_event_id,
                                handler_id: window.join_handler_id.clone(),
                                error: format!(
                                    "join window expired: received {}/{} items for batch {}",
                                    window.source_event_ids.len(),
                                    window.source_event_ids.len(), // we don't have target count here
                                    window.batch_id,
                                ),
                                reason: "join_window_expired".to_string(),
                                attempts: 0,
                                events_to_publish: Vec::new(),
                            })
                            .await?;
                    }
                }
            }

            if !processed_any {
                // Check if there are future-dated effects (from backoff or delay).
                // If so, sleep until the earliest one becomes ready instead of exiting.
                if let Some(earliest) = self.store.earliest_pending_effect_at().await? {
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

    /// Try to build a DLQ event using the global mapper.
    /// Returns None if no global mapper is configured or if the mapper errors.
    fn build_global_dlq_event(
        &self,
        execution: &QueuedHandlerExecution,
        error: String,
        reason: &str,
    ) -> Option<Vec<crate::types::EmittedEvent>> {
        let global = self.global_dlq_mapper.as_ref()?;
        let info = crate::handler::DlqTerminalInfo {
            handler_id: execution.handler_id.clone(),
            source_event_type: event_type_short_name(&execution.event_type).to_string(),
            source_event_id: execution.event_id,
            error,
            reason: reason.to_string(),
            attempts: execution.attempts,
            max_attempts: execution.max_attempts,
        };
        match global(info) {
            Ok(emitted) => Some(vec![emitted]),
            Err(e) => {
                tracing::warn!("global on_dlq mapper failed: {}", e);
                None
            }
        }
    }

    /// Hydrate cold aggregates, then persist every event to the global log.
    ///
    /// 1. For each matching aggregator, hydrate cold aggregates from Store
    /// 2. Always append the event (with aggregate metadata if aggregators match)
    async fn persist_and_hydrate(&self, event: &QueuedEvent) -> Result<()> {
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
                    &agg.aggregate_type,
                    agg_id,
                    &key,
                )
                .await?;
            }
        }

        // Step 2: Always append to global log
        let mut metadata = self.event_metadata.clone();
        if let Some(ref hid) = event.handler_id {
            metadata.insert(
                "handler_id".to_string(),
                serde_json::Value::String(hid.clone()),
            );
        }

        self.store
            .append_event(NewEvent {
                event_id: event.event_id,
                parent_id: event.parent_id,
                correlation_id: event.correlation_id,
                event_type: short_name.to_string(),
                payload: event.payload.clone(),
                created_at: event.created_at,
                aggregate_type,
                aggregate_id,
                metadata,
            })
            .await?;

        Ok(())
    }

    /// Hydrate a single aggregate from Store (with optional snapshot acceleration).
    async fn hydrate_aggregate(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        key: &str,
    ) -> Result<()> {
        // Try snapshot first
        if let Some(snapshot) = self
            .store
            .load_snapshot(aggregate_type, aggregate_id)
            .await?
        {
            let agg = self.aggregators.find_first_by_aggregate_type(aggregate_type);
            if let Some(agg) = agg {
                let mut state = agg.deserialize_state(snapshot.state)?;

                // Load remaining events after snapshot position
                let remaining = self
                    .store
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

        // No snapshot — full replay
        let events = self
            .store
            .load_stream(aggregate_type, aggregate_id)
            .await?;
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

    async fn publish_event<E>(
        &self,
        event: E,
        correlation_id_override: Option<Uuid>,
    ) -> Result<ProcessHandle>
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
        let correlation_id = correlation_id_override.unwrap_or_else(Uuid::new_v4);
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
            handler_id: None,
            created_at: chrono::Utc::now(),
        };

        self.store.publish(queued).await?;

        Ok(ProcessHandle {
            correlation_id,
            event_id,
        })
    }

    async fn publish_output(
        &self,
        output: crate::handler::EventOutput,
        correlation_id_override: Option<Uuid>,
    ) -> Result<ProcessHandle> {
        if let Some(codec) = &output.codec {
            self.effects.register_codec(codec.clone());
        }

        let payload = output.payload;

        let event_id = Uuid::new_v4();
        let correlation_id = correlation_id_override.unwrap_or_else(Uuid::new_v4);

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
            handler_id: None,
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

                self.store
                    .save_snapshot(Snapshot {
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

    /// Build pre-assigned `QueuedEvent`s from `EmittedEvent`s (ID gen lifted into Engine).
    ///
    /// `id_infix` differentiates ID derivation namespaces: `""` for normal
    /// effect output, `"-dlq"` for DLQ terminal events, etc.
    fn build_queued_events(
        emitted: Vec<EmittedEvent>,
        event_id: Uuid,
        handler_id: &str,
        correlation_id: Uuid,
        parent_hops: i32,
        id_infix: &str,
    ) -> Vec<QueuedEvent> {
        emitted
            .into_iter()
            .enumerate()
            .map(|(idx, e)| {
                let new_event_id = Uuid::new_v5(
                    &NAMESPACE_SEESAW,
                    format!(
                        "{}-{}{}-{}-{}",
                        event_id, handler_id, id_infix, e.event_type, idx
                    )
                    .as_bytes(),
                );
                QueuedEvent {
                    id: 0,
                    event_id: new_event_id,
                    parent_id: Some(event_id),
                    correlation_id,
                    event_type: e.event_type,
                    payload: e.payload,
                    hops: parent_hops + 1,
                    retry_count: 0,
                    batch_id: e.batch_id,
                    batch_index: e.batch_index,
                    batch_size: e.batch_size,
                    handler_id: e.handler_id,
                    created_at: chrono::Utc::now(),
                }
            })
            .collect()
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
            .join_append_and_maybe_claim(JoinAppendParams {
                join_handler_id: handler_id.to_string(),
                correlation_id: execution.correlation_id,
                source_event_id: event_id,
                source_event_type: execution.event_type.clone(),
                source_payload: execution.event_payload.clone(),
                source_created_at: execution.execute_at,
                batch_id,
                batch_index,
                batch_size,
                join_window_timeout_seconds: join_window_timeout,
            })
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
                            let handler_config = HandlerWorkerConfig::default();
                            let emitted_events = executor.serialize_emitted_events(
                                outputs,
                                execution,
                                handler_config.max_batch_size,
                            )?;

                            let events_to_publish = Self::build_queued_events(
                                emitted_events,
                                event_id,
                                handler_id,
                                execution.correlation_id,
                                execution.hops,
                                "",
                            );

                            self.store
                                .complete_effect(EffectCompletion {
                                    event_id,
                                    handler_id: handler_id.to_string(),
                                    result: serde_json::json!({ "status": "join_completed" }),
                                    events_to_publish,
                                })
                                .await?;

                            self.store
                                .join_complete(
                                    handler_id.to_string(),
                                    execution.correlation_id,
                                    batch_id,
                                )
                                .await?;
                        }
                        Err(e) => {
                            let error_str = e.to_string();
                            self.store
                                .join_release(
                                    handler_id.to_string(),
                                    execution.correlation_id,
                                    batch_id,
                                    error_str.clone(),
                                )
                                .await?;
                            let dlq_events = self
                                .build_global_dlq_event(
                                    execution,
                                    error_str.clone(),
                                    "join_handler_error",
                                )
                                .unwrap_or_default();
                            let events_to_publish = Self::build_queued_events(
                                dlq_events,
                                event_id,
                                handler_id,
                                execution.correlation_id,
                                execution.hops,
                                "-dlq",
                            );
                            self.store
                                .dlq_effect(EffectDlq {
                                    event_id,
                                    handler_id: handler_id.to_string(),
                                    error: error_str,
                                    reason: "join_handler_error".to_string(),
                                    attempts: 0,
                                    events_to_publish,
                                })
                                .await?;
                        }
                    }
                } else {
                    self.store
                        .complete_effect(EffectCompletion {
                            event_id,
                            handler_id: handler_id.to_string(),
                            result: serde_json::json!({ "status": "join_handler_not_found" }),
                            events_to_publish: Vec::new(),
                        })
                        .await?;
                }
            }
            None => {
                // Still waiting for more items — mark complete (no-op)
                self.store
                    .complete_effect(EffectCompletion {
                        event_id,
                        handler_id: handler_id.to_string(),
                        result: serde_json::json!({ "status": "join_waiting" }),
                        events_to_publish: Vec::new(),
                    })
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
            snapshot_every: self.snapshot_every,
            event_metadata: self.event_metadata.clone(),
            global_dlq_mapper: self.global_dlq_mapper.clone(),
        }
    }
}
