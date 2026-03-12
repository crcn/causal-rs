//! Store-agnostic Engine with built-in settle loop.
//!
//! Engine<D> reads events from an [`EventLog`](crate::event_log::EventLog),
//! distributes reactor work via a [`ReactorQueue`](crate::reactor_queue::ReactorQueue),
//! and settles the full causal tree synchronously.

use std::sync::Arc;
use uuid::Uuid;

use anyhow::Result;
use tracing::info;

use crate::aggregator::{Aggregate, Aggregator, AggregatorRegistry, Apply};
use crate::event_log::EventLog;
use crate::reactor::{GlobalDlqMapper, Reactor, Projection};
use crate::reactor_queue::ReactorQueue;
use crate::reactor_registry::ReactorRegistry;
use crate::job_executor::{ReactorStatus, JobExecutor};
use crate::memory_store::MemoryStore;
use crate::process::{EmitFuture, ProcessHandle};
use crate::types::{
    EmittedEvent, EventWorkerConfig, LogCursor, ReactorCompletion, ReactorDlq,
    ReactorResolution, ReactorWorkerConfig, IntentCommit, NewEvent, PersistedEvent,
    QueuedReactor, Snapshot, StreamVersion, NAMESPACE_CAUSAL,
};
use crate::upcaster::{Upcaster, UpcasterRegistry};

/// Store-agnostic Engine with built-in settle loop.
///
/// Reads events from an [`EventLog`], distributes reactor work via a
/// [`ReactorQueue`], and drives the full causal tree to completion.
///
/// Supply custom implementations via [`new`](Engine::new), or use
/// [`in_memory`](Engine::in_memory) for tests and simple use cases.
pub struct Engine<D>
where
    D: Send + Sync + 'static,
{
    log: Arc<dyn EventLog>,
    queue: Arc<dyn ReactorQueue>,
    deps: Arc<D>,
    reactors: Arc<ReactorRegistry<D>>,
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
    /// Create an engine with in-memory event log and reactor queue.
    ///
    /// Use `.with_store(store)` to swap in a durable backend.
    pub fn new(deps: D) -> Self {
        let store = Arc::new(MemoryStore::new());
        Self {
            log: store.clone(),
            queue: store,
            deps: Arc::new(deps),
            reactors: Arc::new(ReactorRegistry::new()),
            aggregators: Arc::new(AggregatorRegistry::new()),
            upcasters: Arc::new(UpcasterRegistry::new()),
            snapshot_every: None,
            event_metadata: serde_json::Map::new(),
            global_dlq_mapper: None,

        }
    }

    /// Create an engine with explicit event log and reactor queue backends.
    pub fn with_backends(
        deps: D,
        log: Arc<dyn EventLog>,
        queue: Arc<dyn ReactorQueue>,
    ) -> Self {
        Self {
            log,
            queue,
            deps: Arc::new(deps),
            reactors: Arc::new(ReactorRegistry::new()),
            aggregators: Arc::new(AggregatorRegistry::new()),
            upcasters: Arc::new(UpcasterRegistry::new()),
            snapshot_every: None,
            event_metadata: serde_json::Map::new(),
            global_dlq_mapper: None,

        }
    }

    /// Alias for `new()` — creates an engine with in-memory backends.
    pub fn in_memory(deps: D) -> Self {
        Self::new(deps)
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

    /// Invalidate cached aggregate state, forcing re-hydration from the EventLog.
    ///
    /// Use after ingesting foreign events (e.g. from another node) so the
    /// next settle loop rebuilds the aggregate from the persistent log.
    pub fn invalidate_aggregate<A: Aggregate>(&self, id: Uuid) {
        let key = format!("{}:{}", A::aggregate_type(), id);
        self.aggregators.remove_state(&key);
    }

    /// Enable auto-checkpoint snapshots every N events.
    pub fn snapshot_every(mut self, events: u64) -> Self {
        self.snapshot_every = Some(events);
        self
    }

    /// Set metadata to stamp on every persisted event.
    ///
    /// Metadata travels with the event through the EventLog, letting
    /// adapters pull application-level context (e.g. `run_id`, `schema_v`,
    /// `actor`) without holding state themselves.
    ///
    /// ```ignore
    /// let engine = Engine::in_memory(deps)
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

    /// Supply a single backend that implements both `EventLog` and `ReactorQueue`.
    ///
    /// Convenience for stores (like `MemoryStore` or `PostgresStore`) that
    /// handle both responsibilities in one type.
    pub fn with_store<S: EventLog + ReactorQueue + 'static>(mut self, store: Arc<S>) -> Self {
        self.log = store.clone();
        self.queue = store;
        self
    }

    /// Register a global DLQ callback that fires when any reactor exhausts retries.
    ///
    /// The callback receives [`DlqTerminalInfo`] with reactor and event metadata,
    /// and returns a serializable event that gets dispatched into the causal chain.
    ///
    /// Per-reactor `on_failure` takes precedence when present.
    ///
    /// ```ignore
    /// let engine = Engine::in_memory(deps)
    ///     .on_dlq(|info: DlqTerminalInfo| HandlerFailed {
    ///         reactor_id: info.reactor_id.clone(),
    ///         error: info.error.clone(),
    ///     });
    /// ```
    pub fn on_dlq<E, F>(mut self, mapper: F) -> Self
    where
        E: crate::event::Event,
        F: Fn(crate::reactor::DlqTerminalInfo) -> E + Send + Sync + 'static,
    {
        self.global_dlq_mapper = Some(Arc::new(move |info| {
            let failed_reactor_id = info.reactor_id.clone();
            let event = mapper(info);
            Ok(crate::EmittedEvent {
                durable_name: event.durable_name().to_string(),
                event_prefix: E::event_prefix().to_string(),
                persistent: !E::is_ephemeral(),
                payload: serde_json::to_value(&event)?,
                reactor_id: Some(failed_reactor_id),
                ephemeral: None,
            })
        }));
        self
    }

    /// Register a reactor.
    pub fn with_reactor(mut self, reactor: Reactor<D>) -> Self {
        Arc::get_mut(&mut self.reactors)
            .expect("Cannot add reactor after cloning")
            .register(reactor);
        self
    }

    /// Register a projection.
    ///
    /// Projections receive ALL events, return `Result<()>`, and run
    /// sequentially before other reactors.
    pub fn with_projection(mut self, projection: Projection<D>) -> Self {
        Arc::get_mut(&mut self.reactors)
            .expect("Cannot add projection after cloning")
            .register_projection(projection);
        self
    }

    /// Register multiple reactors.
    pub fn with_reactors<I>(mut self, reactors: I) -> Self
    where
        I: IntoIterator<Item = Reactor<D>>,
    {
        let registry = Arc::get_mut(&mut self.reactors).expect("Cannot add reactors after cloning");
        for reactor in reactors {
            registry.register(reactor);
        }
        self
    }

    /// Register an aggregator: maintains live in-memory state for aggregate `A`.
    ///
    /// `extract_id` maps the event to the aggregate ID it belongs to.
    ///
    /// ```ignore
    /// let engine = Engine::in_memory(deps)
    ///     .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
    ///     .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id);
    /// ```
    pub fn with_aggregator<E, A, F>(mut self, extract_id: F) -> Self
    where
        E: crate::event::Event,
        A: Aggregate + Apply<E> + serde::Serialize + serde::de::DeserializeOwned,
        F: Fn(&E) -> Uuid + Send + Sync + 'static,
    {
        // Register event codec so emitted events of this type can be deserialized
        let codec = Arc::new(crate::event_codec::EventCodec {
            event_prefix: E::event_prefix().to_string(),
            type_id: std::any::TypeId::of::<E>(),
            decode: Arc::new(|payload| {
                let event: E = serde_json::from_value(payload.clone())?;
                Ok(Arc::new(event))
            }),
        });
        Arc::get_mut(&mut self.reactors)
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
    ///
    /// ```ignore
    /// let engine = Engine::in_memory(deps)
    ///     .with_upcaster::<OrderPlaced>(1, |mut v: serde_json::Value| {
    ///         v["currency"] = serde_json::json!("USD");
    ///         Ok(v)
    ///     });
    /// ```
    pub fn with_upcaster<E, F>(mut self, from_version: u32, transform: F) -> Self
    where
        E: crate::event::Event,
        F: Fn(serde_json::Value) -> anyhow::Result<serde_json::Value> + Send + Sync + 'static,
    {
        Arc::get_mut(&mut self.upcasters)
            .expect("Cannot add upcaster after cloning")
            .register(Upcaster {
                event_prefix: E::event_prefix().to_string(),
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
        E: crate::event::Event,
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
    pub fn emit_output(&self, output: crate::reactor::EventOutput) -> EmitFuture {
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

    /// Cancel a running workflow by correlation ID.
    pub async fn cancel(&self, correlation_id: Uuid) -> Result<()> {
        self.queue.cancel(correlation_id).await
    }

    /// Return a summary of pending work for a correlation ID.
    pub async fn status(&self, correlation_id: Uuid) -> Result<crate::types::QueueStatus> {
        self.queue.status(correlation_id).await
    }

    /// Drive all pending events and reactors to completion.
    pub async fn settle(&self) -> Result<()> {
        let executor = JobExecutor::new(
            self.deps.clone(),
            self.queue.clone(),
            self.reactors.clone(),
            self.aggregators.clone(),
            self.upcasters.clone(),
            self.global_dlq_mapper.clone(),
        );
        let event_config = EventWorkerConfig::default();
        let handler_config = ReactorWorkerConfig::default();

        // In-memory event retry counter (resets on process restart)
        let mut event_attempts: std::collections::HashMap<LogCursor, u32> = std::collections::HashMap::new();
        // Ephemeral cache: populated from PersistedEvent in Phase 1, injected in Phase 2
        let mut ephemerals: std::collections::HashMap<Uuid, Arc<dyn std::any::Any + Send + Sync>> =
            std::collections::HashMap::new();

        loop {
            let mut processed_any = false;

            // Reclaim stale reactors (running longer than timeout)
            self.queue.reclaim_stale().await?;

            // ── Phase 1: Read new events from the log ──────────────────
            let checkpoint = self.queue.checkpoint().await?;
            let events = self.log.load_from(checkpoint, 1000).await?;

            let mut cancelled_cache = std::collections::HashSet::new();

            for event in events {
                processed_any = true;

                // Cache ephemeral for Phase 2
                if let Some(eph) = &event.ephemeral {
                    ephemerals.insert(event.event_id, eph.clone());
                }

                // Cancellation check
                let is_cancelled = if cancelled_cache.contains(&event.correlation_id) {
                    true
                } else if self.queue.is_cancelled(event.correlation_id).await? {
                    cancelled_cache.insert(event.correlation_id);
                    true
                } else {
                    false
                };
                if is_cancelled {
                    info!(
                        correlation_id = %event.correlation_id,
                        event_id = %event.event_id,
                        event_type = %event.event_type,
                        "Skipping event: workflow cancelled",
                    );
                    self.queue
                        .enqueue(IntentCommit::skip(&event))
                        .await?;
                    continue;
                }

                // Hop check (from metadata)
                let hops = event
                    .metadata
                    .get("_hops")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32;
                if hops >= event_config.max_hops {
                    self.queue
                        .enqueue(IntentCommit::park(
                            &event,
                            format!(
                                "Event exceeded maximum hop count ({}) - infinite loop detected",
                                event_config.max_hops
                            ),
                        ))
                        .await?;
                    event_attempts.remove(&event.position);
                    continue;
                }

                // Event-level retry counter
                let attempts = event_attempts.entry(event.position).or_insert(0);
                *attempts += 1;
                if *attempts > event_config.max_event_retry_attempts as u32 {
                    self.queue
                        .enqueue(IntentCommit::park(
                            &event,
                            format!(
                                "Event failed after {} retry attempts",
                                event_config.max_event_retry_attempts
                            ),
                        ))
                        .await?;
                    event_attempts.remove(&event.position);
                    continue;
                }

                // Persistent events: hydrate aggregates, apply state, snapshot
                // Ephemeral events: skip aggregators/projections (not domain facts)
                let skip_projections = !event.persistent;

                if event.persistent {
                    // Hydrate cold aggregates before processing
                    self.hydrate_for_event(&event).await?;

                    // Apply event to live aggregator state
                    self.apply_to_aggregators(&event);

                    // Auto-checkpoint snapshots if configured
                    if let Some(threshold) = self.snapshot_every {
                        self.maybe_auto_snapshot(&event, threshold).await?;
                    }
                }

                // Process event: match reactors, run projections, build intents
                match executor.process_event_inner(&event, &event_config, skip_projections).await {
                    Ok(commit) => {
                        self.queue.enqueue(commit).await?;
                        event_attempts.remove(&event.position);
                    }
                    Err(_e) => {
                        // Error will be retried on next loop (retry counter tracks attempts)
                        break;
                    }
                }
            }

            // ── Phase 2: Execute reactors ──────────────────────────────
            let mut executions = Vec::new();
            while let Some(mut h) = self.queue.dequeue().await? {
                // Inject ephemeral from cache
                h.ephemeral = ephemerals.get(&h.event_id).cloned();
                executions.push(h);
            }

            if !executions.is_empty() {
                processed_any = true;

                // Cancellation checkpoint 2: batch-check unique correlation IDs,
                // then DLQ cancelled reactors before execution.
                let unique_ids: std::collections::HashSet<Uuid> =
                    executions.iter().map(|e| e.correlation_id).collect();
                let mut cancelled_ids = std::collections::HashSet::new();
                for id in unique_ids {
                    if cancelled_cache.contains(&id)
                        || self.queue.is_cancelled(id).await?
                    {
                        cancelled_ids.insert(id);
                    }
                }

                let mut active_executions = Vec::new();
                for execution in executions {
                    if cancelled_ids.contains(&execution.correlation_id) {
                        info!(
                            correlation_id = %execution.correlation_id,
                            event_id = %execution.event_id,
                            reactor_id = %execution.reactor_id,
                            "DLQ reactor: workflow cancelled",
                        );
                        self.queue
                            .resolve(ReactorResolution::DeadLetter(ReactorDlq {
                                event_id: execution.event_id,
                                reactor_id: execution.reactor_id.clone(),
                                error: "cancelled".to_string(),
                                reason: "cancelled".to_string(),
                                attempts: execution.attempts,

                                log_entries: Vec::new(),
                            }))
                            .await?;
                    } else {
                        active_executions.push(execution);
                    }
                }
                let executions = active_executions;

                // Hydrate cold aggregates before reactor execution
                for aggregate_type in self.aggregators.unique_aggregate_types() {
                    let singleton_key = format!("{}:{}", aggregate_type, Uuid::nil());
                    if !self.aggregators.has_state(&singleton_key) {
                        self.hydrate_aggregate(aggregate_type, Uuid::nil(), &singleton_key, None).await?;
                    }
                }
                for execution in &executions {
                    let matching = self.aggregators.find_by_durable_name(&execution.event_type);
                    for agg in &matching {
                        let agg_id = match agg.extract_id_from_json(&execution.event_payload) {
                            Some(id) => id,
                            None => continue,
                        };
                        let key = format!("{}:{}", agg.aggregate_type, agg_id);
                        if !self.aggregators.has_state(&key) {
                            self.hydrate_aggregate(&agg.aggregate_type, agg_id, &key, None).await?;
                        }
                    }
                }

                let handler_futures: Vec<_> = executions
                    .iter()
                    .map(|execution| {
                        executor.execute_reactor(execution.clone(), &handler_config)
                    })
                    .collect();

                let handler_results = futures::future::join_all(handler_futures).await;

                for (exec_result, execution_clone) in
                    handler_results.into_iter().zip(executions)
                {
                    let event_id = execution_clone.event_id;
                    let reactor_id = execution_clone.reactor_id.clone();

                    match exec_result {
                        Ok(result) => match result.status {
                            ReactorStatus::Success => {
                                // Append emitted events to the log FIRST
                                let mut new_events = Self::build_new_events(
                                    result.emitted_events,
                                    event_id,
                                    &reactor_id,
                                    execution_clone.correlation_id,
                                    execution_clone.hops,
                                    "",
                                    &self.event_metadata,
                                );
                                self.append_emitted_events(&mut new_events, &mut ephemerals)
                                    .await?;
                                // THEN resolve reactor
                                self.queue
                                    .resolve(ReactorResolution::Complete(ReactorCompletion {
                                        event_id,
                                        reactor_id,
                                        result: result.result,

                                        log_entries: result.log_entries,
                                    }))
                                    .await?;
                            }
                            ReactorStatus::Failed { error, attempts } => {
                                let mut dlq_events = Self::build_new_events(
                                    result.emitted_events,
                                    event_id,
                                    &reactor_id,
                                    execution_clone.correlation_id,
                                    execution_clone.hops,
                                    "-dlq",
                                    &self.event_metadata,
                                );
                                self.append_emitted_events(&mut dlq_events, &mut ephemerals)
                                    .await?;
                                self.queue
                                    .resolve(ReactorResolution::DeadLetter(ReactorDlq {
                                        event_id,
                                        reactor_id,
                                        error,
                                        reason: "failed".to_string(),
                                        attempts,

                                        log_entries: result.log_entries,
                                    }))
                                    .await?;
                            }
                            ReactorStatus::Retry { error, attempts } => {
                                let mut next_execute_at = chrono::Utc::now();
                                if let Some(h) =
                                    executor.reactor_registry().find_by_id(&reactor_id)
                                {
                                    if let Some(base) = h.backoff {
                                        let multiplier =
                                            2u64.saturating_pow(attempts.max(1) as u32 - 1);
                                        let delay = base.saturating_mul(multiplier as u32);
                                        next_execute_at = chrono::Utc::now()
                                            + chrono::Duration::from_std(delay)
                                                .unwrap_or(chrono::Duration::seconds(60));
                                    }
                                }
                                self.queue
                                    .resolve(ReactorResolution::Retry {
                                        event_id,
                                        reactor_id,
                                        error,
                                        new_attempts: attempts + 1,
                                        next_execute_at,
                                    })
                                    .await?;
                            }
                            ReactorStatus::Timeout => {
                                let dlq_events_raw = self
                                    .build_global_dlq_event(
                                        &execution_clone,
                                        "Reactor execution timed out".to_string(),
                                        "timeout",
                                    )
                                    .unwrap_or_default();
                                let mut dlq_events = Self::build_new_events(
                                    dlq_events_raw,
                                    event_id,
                                    &reactor_id,
                                    execution_clone.correlation_id,
                                    execution_clone.hops,
                                    "-dlq",
                                    &self.event_metadata,
                                );
                                self.append_emitted_events(&mut dlq_events, &mut ephemerals)
                                    .await?;
                                self.queue
                                    .resolve(ReactorResolution::DeadLetter(ReactorDlq {
                                        event_id,
                                        reactor_id,
                                        error: "timeout".to_string(),
                                        reason: "timeout".to_string(),
                                        attempts: 0,

                                        log_entries: result.log_entries,
                                    }))
                                    .await?;
                            }
                        },
                        Err(e) => {
                            let error_str = e.to_string();
                            let dlq_events_raw = self
                                .build_global_dlq_event(
                                    &execution_clone,
                                    error_str.clone(),
                                    "settle_handler_error",
                                )
                                .unwrap_or_default();
                            let mut dlq_events = Self::build_new_events(
                                dlq_events_raw,
                                event_id,
                                &reactor_id,
                                execution_clone.correlation_id,
                                execution_clone.hops,
                                "-dlq",
                                &self.event_metadata,
                            );
                            self.append_emitted_events(&mut dlq_events, &mut ephemerals)
                                .await?;
                            self.queue
                                .resolve(ReactorResolution::DeadLetter(ReactorDlq {
                                    event_id,
                                    reactor_id,
                                    error: error_str,
                                    reason: "settle_handler_error".to_string(),
                                    attempts: 0,

                                    log_entries: Vec::new(),
                                }))
                                .await?;
                        }
                    }
                }
            }

            // ── Phase 3: Check if settled ──────────────────────────────
            if !processed_any {
                if let Some(earliest) = self.queue.earliest_pending_at().await? {
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
    fn build_global_dlq_event(
        &self,
        execution: &QueuedReactor,
        error: String,
        reason: &str,
    ) -> Option<Vec<crate::types::EmittedEvent>> {
        let global = self.global_dlq_mapper.as_ref()?;
        let info = crate::reactor::DlqTerminalInfo {
            reactor_id: execution.reactor_id.clone(),
            source_event_type: execution.event_type.clone(),
            source_event_id: execution.event_id,
            error,
            reason: reason.to_string(),
            attempts: execution.attempts,
            max_attempts: execution.max_attempts,
        };
        match global(info) {
            Ok(mut emitted) => {
                if emitted.reactor_id.is_none() {
                    emitted.reactor_id = Some(execution.reactor_id.clone());
                }
                Some(vec![emitted])
            }
            Err(e) => {
                tracing::warn!("global on_dlq mapper failed: {}", e);
                None
            }
        }
    }

    /// Hydrate cold aggregates for a PersistedEvent before processing.
    ///
    /// Excludes the current event from hydration — it will be applied
    /// separately by `apply_to_aggregators` to produce correct prev/next state.
    async fn hydrate_for_event(&self, event: &PersistedEvent) -> Result<()> {
        let matching = self.aggregators.find_by_durable_name(&event.event_type);

        for agg in &matching {
            let agg_id = match agg.extract_id_from_json(&event.payload) {
                Some(id) => id,
                None => continue,
            };

            let key = format!("{}:{}", agg.aggregate_type, agg_id);

            if !self.aggregators.has_state(&key) {
                self.hydrate_aggregate(&agg.aggregate_type, agg_id, &key, Some(event.position)).await?;
            }
        }

        Ok(())
    }

    /// Hydrate a single aggregate from EventLog (with optional snapshot acceleration).
    ///
    /// When `exclude_position` is `Some(pos)`, events at or beyond that global
    /// position are excluded — prevents double-apply when the current event is
    /// already in the log but hasn't been applied to aggregator state yet.
    async fn hydrate_aggregate(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        key: &str,
        exclude_position: Option<LogCursor>,
    ) -> Result<()> {
        // Try snapshot first
        if let Some(snapshot) = self
            .log
            .load_snapshot(aggregate_type, aggregate_id)
            .await?
        {
            let agg = self.aggregators.find_first_by_aggregate_type(aggregate_type);
            if let Some(agg) = agg {
                let mut state = agg.deserialize_state(snapshot.state)?;

                let remaining: Vec<_> = self
                    .log
                    .load_stream(aggregate_type, aggregate_id, Some(snapshot.version))
                    .await?
                    .into_iter()
                    .filter(|e| exclude_position.map_or(true, |pos| e.position < pos))
                    .collect();

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

                // Use actual stream version from last replayed event, not arithmetic
                let final_version = remaining
                    .last()
                    .and_then(|e| e.version)
                    .unwrap_or(snapshot.version);
                self.aggregators
                    .set_state(key, Arc::from(state), final_version, snapshot.version);
                return Ok(());
            }
        }

        // No snapshot — full replay (excluding current event)
        let events: Vec<_> = self
            .log
            .load_stream(aggregate_type, aggregate_id, None)
            .await?
            .into_iter()
            .filter(|e| exclude_position.map_or(true, |pos| e.position < pos))
            .collect();
        if events.is_empty() {
            return Ok(());
        }

        let event_pairs: Vec<(&str, &serde_json::Value)> = events
            .iter()
            .map(|e| (e.event_type.as_str(), &e.payload))
            .collect();

        let last_version = events
            .last()
            .unwrap()
            .version
            .ok_or_else(|| anyhow::anyhow!("aggregate stream event missing version"))?;

        if let Some(state) = self.aggregators.replay_events(
            aggregate_type,
            &event_pairs,
            &self.upcasters,
        )? {
            self.aggregators
                .set_state(key, Arc::from(state), last_version, StreamVersion::ZERO);
        }

        Ok(())
    }

    async fn publish_event<E>(
        &self,
        event: E,
        correlation_id_override: Option<Uuid>,
    ) -> Result<ProcessHandle>
    where
        E: crate::event::Event,
    {
        // Auto-register codec so the event can be decoded in the settle loop
        let codec = Arc::new(crate::event_codec::EventCodec {
            event_prefix: E::event_prefix().to_string(),
            type_id: std::any::TypeId::of::<E>(),
            decode: Arc::new(|payload| {
                let event: E = serde_json::from_value(payload.clone())?;
                Ok(Arc::new(event))
            }),
        });
        self.reactors.register_codec(codec);

        let event_id = Uuid::new_v4();
        let correlation_id = correlation_id_override.unwrap_or_else(Uuid::new_v4);
        let durable_name = event.durable_name().to_string();
        let payload = serde_json::to_value(&event).expect("Event must be serializable");
        let ephemeral: Arc<dyn std::any::Any + Send + Sync> = Arc::new(event);

        info!(
            "Publishing event: type={}, correlation_id={}",
            durable_name, correlation_id
        );

        // Determine aggregate metadata from matching aggregators
        let matching = self.aggregators.find_by_durable_name(&durable_name);
        let (aggregate_type, aggregate_id) = matching
            .iter()
            .find_map(|agg| {
                agg.extract_id_from_json(&payload)
                    .map(|id| (agg.aggregate_type.clone(), id))
            })
            .map(|(t, id)| (Some(t), Some(id)))
            .unwrap_or((None, None));

        let metadata = self.event_metadata.clone();

        let persistent = !E::is_ephemeral();

        let new_event = NewEvent {
            event_id,
            parent_id: None,
            correlation_id,
            event_type: durable_name,
            payload,
            created_at: chrono::Utc::now(),
            aggregate_type,
            aggregate_id,
            metadata,
            ephemeral: Some(ephemeral),
            persistent,
        };

        self.log.append(new_event).await?;

        Ok(ProcessHandle {
            correlation_id,
            event_id,
            queue: Some(self.queue.clone()),
        })
    }

    async fn publish_output(
        &self,
        output: crate::reactor::EventOutput,
        correlation_id_override: Option<Uuid>,
    ) -> Result<ProcessHandle> {
        if let Some(codec) = &output.codec {
            self.reactors.register_codec(codec.clone());
        }

        let event_id = Uuid::new_v4();
        let correlation_id = correlation_id_override.unwrap_or_else(Uuid::new_v4);

        info!(
            "Publishing event output: type={}, correlation_id={}",
            output.durable_name, correlation_id
        );

        // Determine aggregate metadata
        let matching = self.aggregators.find_by_durable_name(&output.durable_name);
        let (aggregate_type, aggregate_id) = matching
            .iter()
            .find_map(|agg| {
                agg.extract_id_from_json(&output.payload)
                    .map(|id| (agg.aggregate_type.clone(), id))
            })
            .map(|(t, id)| (Some(t), Some(id)))
            .unwrap_or((None, None));

        let metadata = self.event_metadata.clone();

        let new_event = NewEvent {
            event_id,
            parent_id: None,
            correlation_id,
            event_type: output.durable_name,
            payload: output.payload,
            created_at: chrono::Utc::now(),
            aggregate_type,
            aggregate_id,
            metadata,
            ephemeral: output.ephemeral,
            persistent: output.persistent,
        };

        self.log.append(new_event).await?;

        Ok(ProcessHandle {
            correlation_id,
            event_id,
            queue: Some(self.queue.clone()),
        })
    }

    /// Resolve aggregate metadata and append events to the EventLog.
    async fn append_emitted_events(
        &self,
        new_events: &mut [NewEvent],
        ephemerals: &mut std::collections::HashMap<Uuid, Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Result<()> {
        for new_event in new_events.iter_mut() {
            if !new_event.persistent {
                continue; // Skip aggregate resolution for ephemeral events
            }
            // Resolve aggregate metadata (persistent events only)
            let matching = self.aggregators.find_by_durable_name(&new_event.event_type);
            if let Some((agg_type, agg_id)) = matching
                .iter()
                .find_map(|agg| {
                    agg.extract_id_from_json(&new_event.payload)
                        .map(|id| (agg.aggregate_type.clone(), id))
                })
            {
                new_event.aggregate_type = Some(agg_type);
                new_event.aggregate_id = Some(agg_id);
            }
        }
        for new_event in new_events.iter() {
            self.log.append(new_event.clone()).await?;
            if let Some(eph) = &new_event.ephemeral {
                ephemerals.insert(new_event.event_id, eph.clone());
            }
        }
        Ok(())
    }

    /// Auto-snapshot aggregates if the event count since last snapshot exceeds the threshold.
    async fn maybe_auto_snapshot(
        &self,
        event: &PersistedEvent,
        threshold: u64,
    ) -> Result<()> {
        let matching = self.aggregators.find_by_durable_name(&event.event_type);

        for agg in matching {
            let aggregate_id = match agg.extract_id_from_json(&event.payload) {
                Some(id) => id,
                None => continue,
            };

            let key = format!("{}:{}", agg.aggregate_type, aggregate_id);
            let version = self.aggregators.get_version(&key);
            let snapshot_at = self.aggregators.get_snapshot_at_version(&key);

            if version.raw() - snapshot_at.raw() >= threshold {
                let state_ref = match self.aggregators.get_state(&key) {
                    Some(s) => s,
                    None => continue,
                };

                let state_json = agg.serialize_state(state_ref.as_ref())?;

                self.log
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
    fn apply_to_aggregators(&self, event: &PersistedEvent) {
        self.aggregators
            .apply_event(&event.event_type, &event.payload);
    }

    /// Build `NewEvent`s from `EmittedEvent`s with deterministic IDs and routing metadata.
    fn build_new_events(
        emitted: Vec<EmittedEvent>,
        event_id: Uuid,
        reactor_id: &str,
        correlation_id: Uuid,
        parent_hops: i32,
        id_infix: &str,
        base_metadata: &serde_json::Map<String, serde_json::Value>,
    ) -> Vec<NewEvent> {
        emitted
            .into_iter()
            .enumerate()
            .map(|(idx, e)| {
                let new_event_id = Uuid::new_v5(
                    &NAMESPACE_CAUSAL,
                    format!(
                        "{}-{}{}-{}-{}",
                        event_id, reactor_id, id_infix, e.durable_name, idx
                    )
                    .as_bytes(),
                );

                let mut metadata = base_metadata.clone();
                // Routing metadata (underscore prefix)
                metadata.insert(
                    "_hops".to_string(),
                    serde_json::Value::Number((parent_hops + 1).into()),
                );
                metadata.insert(
                    "_reactor_id".to_string(),
                    serde_json::Value::String(reactor_id.to_string()),
                );
                if let Some(ref hid) = e.reactor_id {
                    metadata.insert(
                        "reactor_id".to_string(),
                        serde_json::Value::String(hid.clone()),
                    );
                }

                NewEvent {
                    event_id: new_event_id,
                    parent_id: Some(event_id),
                    correlation_id,
                    event_type: e.durable_name,
                    payload: e.payload,
                    created_at: chrono::Utc::now(),
                    aggregate_type: None,
                    aggregate_id: None,
                    metadata,
                    ephemeral: e.ephemeral,
                    persistent: e.persistent,
                }
            })
            .collect()
    }

}

impl<D> Clone for Engine<D>
where
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            log: self.log.clone(),
            queue: self.queue.clone(),
            deps: self.deps.clone(),
            reactors: self.reactors.clone(),
            aggregators: self.aggregators.clone(),
            upcasters: self.upcasters.clone(),
            snapshot_every: self.snapshot_every,
            event_metadata: self.event_metadata.clone(),
            global_dlq_mapper: self.global_dlq_mapper.clone(),
        }
    }
}
