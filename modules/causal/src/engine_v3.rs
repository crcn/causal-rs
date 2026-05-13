//! v0.3 Engine + EngineBuilder.
//!
//! Wires the per-consumer runners (Phase 4b/4c) into a public `Engine`
//! surface that spawns supervisor tasks per consumer plus a relay
//! drain task. Phase 4d MVP: `emit` + `shutdown` only. Aggregate-side
//! `load` / `append` (with OCC), StreamPolicy enforcement on `emit`,
//! and the `ViewHandle` query surface land in Phase 5+ as documented
//! in `docs/plans/2026-05-05-causal-v03-impl-plan.md`.
//!
//! Lives at `crate::engine_v3::Engine` until Phase 9 renames the file
//! and removes the legacy `crate::engine::Engine<D>`. The two coexist.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
#[cfg(test)]
use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::aggregate_v3::{Aggregate, Apply};
use crate::aggregator::{Aggregator, AggregatorRegistry};
use crate::checkpoint_store::{CheckpointStore, ReactorOutbox};
use crate::multi_projector::{MultiProjector, MultiProjectorRunner};
use crate::contexts::Metadata;
use crate::event_log::EventLogBackend;
use crate::fact::Fact;
use crate::projector::Projector;
use crate::projection_runner::{ProjectionRunner, StepOutcome};
use crate::reactor_runner::ReactorRunner;
use crate::reactor_v3::Reactor;
use crate::relay::RelayLoop;
use crate::types::{LogCursor, NewEvent, StreamVersion};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const BACKOFF_ON_ERROR: Duration = Duration::from_millis(250);
const SUPERVISOR_BATCH: usize = 256;
const RELAY_BATCH: usize = 256;

// ─────────────────────────────────────────────────────────────────────
// Supervisable — trait-object adapter so we can box heterogeneous
// runners (ProjectionRunner<M>, ViewRunner<V>, ReactorRunner<R>) into
// a single Vec for the supervisor pool.
// ─────────────────────────────────────────────────────────────────────

#[async_trait]
trait Supervisable: Send + Sync {
    async fn step(&self, batch: usize) -> Result<StepOutcome>;
    fn consumer_id(&self) -> &str;
}

#[async_trait]
impl<P: Projector + 'static> Supervisable for ProjectionRunner<P>
where
    P::Fact: DeserializeOwned,
{
    async fn step(&self, batch: usize) -> Result<StepOutcome> {
        ProjectionRunner::step(self, batch).await
    }
    fn consumer_id(&self) -> &str { ProjectionRunner::consumer_id(self) }
}

#[async_trait]
impl<R: Reactor + 'static> Supervisable for ReactorRunner<R>
where
    R::Trigger: DeserializeOwned,
{
    async fn step(&self, batch: usize) -> Result<StepOutcome> {
        ReactorRunner::step(self, batch).await
    }
    fn consumer_id(&self) -> &str { ReactorRunner::consumer_id(self) }
}

#[async_trait]
impl<P: MultiProjector + 'static> Supervisable for MultiProjectorRunner<P> {
    async fn step(&self, batch: usize) -> Result<StepOutcome> {
        MultiProjectorRunner::step(self, batch).await
    }
    fn consumer_id(&self) -> &str { MultiProjectorRunner::consumer_id(self) }
}

// ─────────────────────────────────────────────────────────────────────
// Bulk registration — trait objects produced by module macros.
// ─────────────────────────────────────────────────────────────────────
//
// `with_projectors`, `with_reactors`, `with_multi_projectors` consume
// `Box<dyn *Registration>` so a module-level helper can return one
// uniform `Vec` of mixed consumer types. The blanket impls below cover
// every `Projector` / `Reactor` / `MultiProjector` automatically — the
// macro just boxes its instances and the bulk method walks the vector.

pub trait ProjectorRegistration: Send + 'static {
    fn register(self: Box<Self>, builder: EngineBuilder) -> EngineBuilder;
}

impl<P> ProjectorRegistration for P
where
    P: Projector + 'static,
    P::Fact: DeserializeOwned,
{
    fn register(self: Box<Self>, builder: EngineBuilder) -> EngineBuilder {
        builder.with_projector(*self)
    }
}

pub trait ReactorRegistration: Send + 'static {
    fn register(self: Box<Self>, builder: EngineBuilder) -> EngineBuilder;
}

impl<R> ReactorRegistration for R
where
    R: Reactor + 'static,
    R::Trigger: DeserializeOwned,
{
    fn register(self: Box<Self>, builder: EngineBuilder) -> EngineBuilder {
        builder.with_reactor(*self)
    }
}

pub trait MultiProjectorRegistration: Send + 'static {
    fn register(self: Box<Self>, builder: EngineBuilder) -> EngineBuilder;
}

impl<P> MultiProjectorRegistration for P
where
    P: MultiProjector + 'static,
{
    fn register(self: Box<Self>, builder: EngineBuilder) -> EngineBuilder {
        builder.with_multi_projector(*self)
    }
}

/// Result of a successful `emit(...).await`.
///
/// `position` is the global log cursor of the last event written
/// (single emits and batches alike). For an empty-batch emit,
/// `position` is the log's current latest position so a
/// downstream `settle(result)` waits for any pre-existing pending
/// work to drain.
///
/// `correlation_id` is the chain id stamped on every fact in the
/// batch. Clients use it to poll workflow-status projections;
/// tests use it to scope reads (`engine.snapshot::<A>(corr_id)`)
/// and waits (`engine.settled()`) to a single chain. Auto-generated
/// per emit unless the caller set it via `EmitBuilder::correlation_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmitResult {
    pub position:       LogCursor,
    pub correlation_id: Uuid,
}

/// Metadata about a reactor that has exhausted its retry budget,
/// passed to the [`EngineBuilder::on_dlq`] mapper. The mapper
/// decides whether to synthesize a terminal-failure Fact and emit
/// it through the outbox so downstream consumers can react.
///
/// Production use case: scout's `PipelineEvent::HandlerFailed` is
/// emitted on terminal reactor failure so `PipelineState` can fold
/// it and unblock downstream gates.
#[derive(Debug, Clone)]
pub struct DlqInfo {
    /// `Reactor::GROUP_NAME` of the failing reactor.
    pub group_name:        String,
    /// `event_id` of the trigger that caused the failure.
    pub source_event_id:   Uuid,
    /// `event_type` of the trigger (canonical `{CATEGORY}:{name}`).
    pub source_event_type: String,
    /// Last error message from the reactor's `react()`.
    pub error:             String,
    /// Number of attempts that ran before declaring terminal
    /// failure (equal to `max_attempts`).
    pub attempts:          u32,
}

/// Type-erased view of a [`Fact`] for the emit builder.
///
/// `EmitInput` stores facts behind this trait so [`EmitBuilder`] is
/// non-generic — one builder type handles any Fact, single or
/// batched. The blanket impl below covers every `Fact` automatically;
/// downstream code never names this trait.
pub(crate) trait ErasedFact: Send + Sync {
    fn category(&self) -> &'static str;
    fn variant_name(&self) -> &str;
    fn stream_id(&self) -> Uuid;
    fn occurred_at(&self) -> Option<DateTime<Utc>>;
    fn to_value(&self) -> Result<serde_json::Value>;
}

impl<F: Fact> ErasedFact for F {
    fn category(&self) -> &'static str { <F as Fact>::CATEGORY }
    fn variant_name(&self) -> &str { Fact::name(self) }
    fn stream_id(&self) -> Uuid { Fact::stream_id(self) }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Fact::occurred_at(self) }
    fn to_value(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).map_err(Into::into)
    }
}

/// What `Engine::emit` accepts: a single Fact, or a batch of Facts
/// of the same type. Both produced automatically via `Into` impls —
/// callers write `engine.emit(fact)` or `engine.emit(vec![f1, f2])`.
pub struct EmitInput {
    facts: Vec<Box<dyn ErasedFact>>,
}

impl<F: Fact> From<F> for EmitInput {
    fn from(f: F) -> Self {
        Self { facts: vec![Box::new(f) as Box<dyn ErasedFact>] }
    }
}

impl<F: Fact> From<Vec<F>> for EmitInput {
    fn from(v: Vec<F>) -> Self {
        Self {
            facts: v.into_iter()
                .map(|f| Box::new(f) as Box<dyn ErasedFact>)
                .collect(),
        }
    }
}

/// Chainable per-emit envelope. Construct via [`Engine::emit`]; finish
/// with `.await`. Methods return `self` so chains compose freely.
///
/// `EmitBuilder` is non-generic — Facts are type-erased at construction
/// time via the `Into<EmitInput>` impls, so one builder type handles
/// any Fact, single or batched.
pub struct EmitBuilder<'a> {
    engine:         &'a Engine,
    input:          EmitInput,
    correlation_id: Option<Uuid>,
    parent_id:      Option<Uuid>,
    metadata:       Metadata,
}

impl<'a> EmitBuilder<'a> {
    /// Stamp `correlation_id` on every fact in the batch. Defaults to
    /// a fresh UUID per emit; command handlers should propagate the
    /// trigger's `correlation_id` here so causal-chain tracing works
    /// across the system.
    pub fn correlation_id(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Stamp `parent_id` on every fact in the batch. Defaults to
    /// `None` (root event). Command handlers should pass the trigger's
    /// `event_id` here.
    pub fn parent_id(mut self, id: Uuid) -> Self {
        self.parent_id = Some(id);
        self
    }

    /// Add a metadata key/value to every fact in the batch. Multiple
    /// `.metadata(...)` calls accumulate. Application code uses this
    /// for `_run_id`, `_schema_v`, `_phase`, etc.
    pub fn metadata<V: Into<serde_json::Value>>(mut self, key: &str, value: V) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Wait for the full causal chain triggered by this emit to drain.
    ///
    /// `engine.emit(fact).await` returns once the fact is durably in
    /// the log; the reactor chain runs asynchronously and may still
    /// be in flight. `engine.emit(fact).settled().await` returns only
    /// after every registered consumer has observed every event
    /// produced by this emit (and any downstream emissions the
    /// reactor chain produced from it).
    ///
    /// Use for tests, sync command handlers, or any case where the
    /// caller needs the side effects to be visible before continuing.
    /// For HTTP handlers that just need to confirm durability and
    /// return a correlation_id to the client, use the bare
    /// `.await` instead — it's faster and avoids holding connections
    /// open during long chains.
    ///
    /// Timeout: wrap with `tokio::time::timeout` as needed.
    ///
    /// ```ignore
    /// // wait up to 5s for the chain to drain
    /// tokio::time::timeout(
    ///     Duration::from_secs(5),
    ///     engine.emit(fact).settled(),
    /// ).await??;
    /// ```
    pub fn settled(self) -> SettledEmit<'a> {
        SettledEmit { builder: self }
    }
}

impl<'a> std::future::IntoFuture for EmitBuilder<'a> {
    type Output = Result<EmitResult>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<EmitResult>> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let engine = self.engine;
            engine.execute_emit(self).await
        })
    }
}

/// Terminal returned by [`EmitBuilder::settled`]. `.await` runs the
/// emit and then waits for the resulting causal chain to drain
/// across every registered consumer.
#[must_use = "settled() returns a future — call .await to run the emit and drain"]
pub struct SettledEmit<'a> {
    builder: EmitBuilder<'a>,
}

impl<'a> std::future::IntoFuture for SettledEmit<'a> {
    type Output = Result<EmitResult>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<EmitResult>> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let engine = self.builder.engine;
            let result = engine.execute_emit(self.builder).await?;
            engine.settle(result).await?;
            Ok(result)
        })
    }
}

// ─────────────────────────────────────────────────────────────────────
// EngineBuilder
// ─────────────────────────────────────────────────────────────────────

/// Constructs an `Arc<dyn Supervisable>` runner once a per-consumer
/// aggregator registry has been built. Stored on the builder so
/// registry creation can happen at `build()` time after every
/// `.with_aggregators(...)` call has accumulated.
type RunnerFactory = Box<
    dyn FnOnce(Option<Arc<AggregatorRegistry>>) -> Arc<dyn Supervisable> + Send,
>;

/// Type-erased DLQ mapper plumbed from
/// `EngineBuilder::on_dlq` into each `ReactorRunner`.
pub(crate) type DlqMapperArc = Arc<
    dyn Fn(DlqInfo) -> Option<Box<dyn ErasedFact>> + Send + Sync,
>;

/// Framework default for `max_attempts` when `on_dlq` is configured.
/// Reactors retry up to this many times before the mapper fires.
pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 3;

pub struct EngineBuilder {
    log:                   Arc<dyn EventLogBackend>,
    checkpoint:            Arc<dyn CheckpointStore>,
    outbox:                Arc<dyn ReactorOutbox>,
    consumers:             Vec<RunnerFactory>,
    aggregators:           Vec<Aggregator>,
    group_names:           std::collections::HashSet<String>,
    default_metadata:      Metadata,
    dlq_mapper:            Option<DlqMapperArc>,
    max_attempts:          u32,
    observer:              Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
}

impl EngineBuilder {
    /// `outbox` and `checkpoint` typically point to the same backend
    /// instance (e.g., one `Arc<MemoryStore>` cast to both traits) so
    /// that C12's atomic outbox+cursor commit holds. Backends that
    /// support only `CheckpointStore` (no reactor outbox) can pass any
    /// `ReactorOutbox` impl that errors on commit_reactor_batch — the
    /// engine is happy as long as no reactors are registered.
    pub fn new(
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
        outbox: Arc<dyn ReactorOutbox>,
    ) -> Self {
        Self {
            log,
            checkpoint,
            outbox,
            consumers: Vec::new(),
            aggregators: Vec::new(),
            group_names: std::collections::HashSet::new(),
            default_metadata: Metadata::new(),
            dlq_mapper: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            observer: None,
        }
    }

    /// Register a [`ReactorObserver`](crate::reactor_observer::ReactorObserver)
    /// for telemetry / inspector capture. Plumbed into every
    /// `ReactorRunner` registered after this call AND into the
    /// engine-level aggregator fold path. Without this, observer
    /// hooks are noop (zero hot-path overhead).
    ///
    /// `MemoryStore` implements `ReactorObserver` directly — the
    /// common in-process pattern is:
    ///
    /// ```ignore
    /// let store = Arc::new(MemoryStore::new());
    /// let engine = EngineBuilder::new(
    ///         store.clone() as Arc<dyn EventLogBackend>,
    ///         store.clone() as Arc<dyn CheckpointStore>,
    ///         store.clone() as Arc<dyn ReactorOutbox>,
    ///     )
    ///     .with_observer(store.clone())
    ///     .with_reactor(MyReactor)
    ///     .build();
    /// ```
    pub fn with_observer<O>(mut self, observer: Arc<O>) -> Self
    where
        O: crate::reactor_observer::ReactorObserver + 'static,
    {
        self.observer = Some(observer as Arc<dyn crate::reactor_observer::ReactorObserver>);
        self
    }

    /// Register a DLQ mapper. When any reactor's `react()` errors
    /// `max_attempts` times in a row on the same trigger event, the
    /// runner stops retrying and invokes the mapper with the
    /// failure details. If the mapper returns `Some(fact)`, the
    /// fact is emitted through the outbox (so downstream consumers
    /// react to it) and the cursor advances past the failing event.
    ///
    /// Without this, terminal failures park forever — the
    /// supervisor backs off + retries indefinitely, blocking
    /// downstream progress.
    ///
    /// Use case in scout: synthesize `PipelineEvent::HandlerFailed`
    /// from `DlqInfo`; `PipelineState` folds it and unblocks
    /// downstream gates based on `info.group_name`.
    pub fn on_dlq<F, Out>(mut self, mapper: F) -> Self
    where
        F: Fn(DlqInfo) -> Option<Out> + Send + Sync + 'static,
        Out: Fact,
    {
        self.dlq_mapper = Some(Arc::new(move |info| {
            mapper(info).map(|f| Box::new(f) as Box<dyn ErasedFact>)
        }));
        self
    }

    /// Override the framework's default retry budget for reactors.
    /// Default is [`DEFAULT_MAX_ATTEMPTS`] (3). Applies only when
    /// `on_dlq` is also configured — without a mapper, reactors
    /// retry indefinitely (supervisor backoff).
    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    /// Stamp these metadata keys on every emit through this engine.
    /// Per-emit `EmitBuilder::metadata(...)` calls override defaults
    /// on key collision; non-colliding keys merge.
    ///
    /// Typical use: `_run_id`, `_actor`, `_schema_v` — values that
    /// belong to every event a service produces, set once at engine
    /// construction.
    pub fn with_default_metadata(mut self, defaults: Metadata) -> Self {
        self.default_metadata = defaults;
        self
    }

    /// Reserve a `GROUP_NAME` for a consumer. Panics if another
    /// consumer in this builder already claimed it — two consumers
    /// sharing a cursor key would silently corrupt each other.
    fn claim_group_name(&mut self, group_name: &'static str) {
        assert!(
            self.group_names.insert(group_name.into()),
            "duplicate GROUP_NAME `{}` registered on EngineBuilder — \
             two consumers MUST NOT share a cursor key",
            group_name,
        );
    }

    /// Register [`crate::Aggregator`] definitions so consumers can read
    /// folded aggregate state via `ctx.aggregate::<A>(id)`. Aggregator
    /// state is **per-engine, in-memory** — it does NOT persist across
    /// `Engine` instances. For saga-pattern read state shared across
    /// reactors within one workflow run, this is the right tool.
    /// For long-lived aggregates spanning processes, see
    /// `docs/aggregate-state-scope.md`.
    ///
    /// Chainable; each call accumulates onto the same set. Each runner
    /// gets its OWN [`AggregatorRegistry`] copy (cheap clones of the
    /// `Aggregator` definitions, independent state) so per-runner folds
    /// don't race with each other.
    ///
    /// Typically fed by the `#[aggregators]` macro:
    /// ```ignore
    /// EngineBuilder::new(...)
    ///     .with_aggregators(my_aggs::aggregators())
    ///     .with_aggregators(other_aggs::aggregators())
    /// ```
    pub fn with_aggregators<I>(mut self, aggregators: I) -> Self
    where
        I: IntoIterator<Item = Aggregator>,
    {
        for agg in aggregators {
            // Reject duplicate Aggregate::NAME within one builder —
            // two aggregators sharing the registry key
            // `{NAME}:{id}` would silently overwrite each other's
            // state. Same protective pattern as `claim_group_name`.
            //
            // Multiple aggregators with the same NAME folding
            // DIFFERENT Fact types into one Aggregate (the multi-Fact
            // Apply<F1> + Apply<F2> case) is legitimate — those
            // SHOULD share a NAME by construction (same A). To
            // distinguish that case from a true collision: assert
            // also that event_prefix differs.
            if let Some(existing) = self.aggregators.iter().find(|a| {
                a.aggregate_type == agg.aggregate_type
                    && a.event_prefix == agg.event_prefix
            }) {
                panic!(
                    "duplicate Aggregate::NAME `{}` registered against the \
                     same Fact CATEGORY `{}` — two aggregators MUST NOT \
                     share a registry key. Multi-Fact aggregates folding \
                     different Fact streams are fine (Apply<F1> + Apply<F2> \
                     with the same A::NAME); same Fact registered twice \
                     is not.",
                    agg.aggregate_type, existing.event_prefix,
                );
            }
            self.aggregators.push(agg);
        }
        self
    }

    pub fn with_projector<P: Projector + 'static>(mut self, p: P) -> Self
    where
        P::Fact: DeserializeOwned,
    {
        self.claim_group_name(P::GROUP_NAME);
        let log = self.log.clone();
        let checkpoint = self.checkpoint.clone();
        let observer = self.observer.clone();
        self.consumers.push(Box::new(move |aggs| {
            let mut runner = ProjectionRunner::new(p, P::GROUP_NAME, log, checkpoint);
            if let Some(aggs) = aggs { runner = runner.with_aggregators(aggs); }
            if let Some(obs) = observer { runner = runner.with_observer(obs); }
            Arc::new(runner) as Arc<dyn Supervisable>
        }));
        self
    }

    pub fn with_reactor<R: Reactor + 'static>(mut self, r: R) -> Self
    where
        R::Trigger: DeserializeOwned,
    {
        self.claim_group_name(R::GROUP_NAME);
        let log = self.log.clone();
        let outbox = self.outbox.clone();
        let dlq_mapper = self.dlq_mapper.clone();
        let max_attempts = self.max_attempts;
        let observer = self.observer.clone();
        self.consumers.push(Box::new(move |aggs| {
            let mut runner = ReactorRunner::new(r, R::GROUP_NAME, log, outbox);
            if let Some(aggs) = aggs { runner = runner.with_aggregators(aggs); }
            if let Some(mapper) = dlq_mapper {
                runner = runner.with_dlq(mapper, max_attempts);
            }
            if let Some(obs) = observer { runner = runner.with_observer(obs); }
            Arc::new(runner) as Arc<dyn Supervisable>
        }));
        self
    }

    /// Register a [`MultiProjector`] — cross-domain projection
    /// consumer with declared subscription. The runner filters events
    /// to those whose `event_type` matches any category in
    /// `P::CATEGORIES` (matching `{CATEGORY}:*`) before invoking the
    /// body. Body receives raw `&PersistedEvent` for cross-domain
    /// payload routing.
    ///
    /// Use when:
    /// - Body needs raw `&PersistedEvent` (heterogeneous payload routing
    ///   that no single typed enum captures), AND
    /// - Subscription is a known-bounded set of categories.
    ///
    /// For single-Fact consumers, use [`Self::with_projector`] — it
    /// deserializes for you.
    pub fn with_multi_projector<P: MultiProjector + 'static>(mut self, p: P) -> Self {
        self.claim_group_name(P::GROUP_NAME);
        let log = self.log.clone();
        let checkpoint = self.checkpoint.clone();
        let observer = self.observer.clone();
        self.consumers.push(Box::new(move |aggs| {
            let mut runner = MultiProjectorRunner::new(p, P::GROUP_NAME, log, checkpoint);
            if let Some(aggs) = aggs { runner = runner.with_aggregators(aggs); }
            if let Some(obs) = observer { runner = runner.with_observer(obs); }
            Arc::new(runner) as Arc<dyn Supervisable>
        }));
        self
    }

    /// Bulk-register projectors yielded by an iterator — typically
    /// from a macro-generated `module::projectors()` helper that
    /// returns `Vec<Box<dyn ProjectorRegistration>>`.
    pub fn with_projectors<I>(self, projectors: I) -> Self
    where
        I: IntoIterator<Item = Box<dyn ProjectorRegistration>>,
    {
        projectors.into_iter().fold(self, |b, p| p.register(b))
    }

    /// Bulk-register reactors yielded by an iterator — typically
    /// from a macro-generated `module::reactors()` helper that
    /// returns `Vec<Box<dyn ReactorRegistration>>`.
    pub fn with_reactors<I>(self, reactors: I) -> Self
    where
        I: IntoIterator<Item = Box<dyn ReactorRegistration>>,
    {
        reactors.into_iter().fold(self, |b, r| r.register(b))
    }

    /// Bulk-register multi-projectors yielded by an iterator.
    pub fn with_multi_projectors<I>(self, multi_projectors: I) -> Self
    where
        I: IntoIterator<Item = Box<dyn MultiProjectorRegistration>>,
    {
        multi_projectors.into_iter().fold(self, |b, p| p.register(b))
    }

    pub fn build(self) -> Engine {
        let aggregators = self.aggregators;
        let make_registry = || -> Option<Arc<AggregatorRegistry>> {
            if aggregators.is_empty() { return None; }
            let mut reg = AggregatorRegistry::new();
            for agg in &aggregators {
                reg.register(agg.clone());
            }
            Some(Arc::new(reg))
        };
        // Engine-level registry for external read access — separate
        // from per-consumer clones so consumer-side capture/restore
        // rollback doesn't leak into outside observers' state.
        let engine_aggregators = make_registry();
        let consumers: Vec<Arc<dyn Supervisable>> = self.consumers
            .into_iter()
            .map(|f| f(make_registry()))
            .collect();
        let consumer_ids: Vec<String> = self.group_names.into_iter().collect();
        Engine::start(
            self.log,
            self.checkpoint,
            self.outbox,
            consumers,
            self.default_metadata,
            engine_aggregators,
            consumer_ids,
            self.observer,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────
// Engine
// ─────────────────────────────────────────────────────────────────────

pub struct Engine {
    log:                   Arc<dyn EventLogBackend>,
    checkpoint:            Arc<dyn CheckpointStore>,
    /// Held alongside the relay's handle so `settle` can check
    /// `outbox_pending` for race-free quiescence detection.
    outbox:                Arc<dyn ReactorOutbox>,
    shutdown_tx:           broadcast::Sender<()>,
    handles:               Vec<JoinHandle<()>>,
    default_metadata:      Metadata,
    /// Engine-level aggregator registry for out-of-band read access
    /// via `engine.snapshot::<A>(stream_id)`. Folded on every
    /// successful `emit()`. Each consumer holds its OWN registry
    /// clone for in-body `ctx.aggregate` reads — the engine-level
    /// registry exists for the test ergonomic of reading aggregate
    /// state without going through a consumer.
    aggregators:           Option<Arc<AggregatorRegistry>>,
    /// Consumer group names registered with the builder, in
    /// registration order. Used by `Engine::settle` to await every
    /// consumer catching up to an emit position.
    consumer_ids:          Vec<String>,
    /// Telemetry hook for inspector / external observability sinks.
    observer:              Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
}

impl Engine {
    fn start(
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
        outbox: Arc<dyn ReactorOutbox>,
        consumers: Vec<Arc<dyn Supervisable>>,
        default_metadata: Metadata,
        aggregators: Option<Arc<AggregatorRegistry>>,
        consumer_ids: Vec<String>,
        observer: Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
    ) -> Self {
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let mut handles = Vec::with_capacity(consumers.len() + 1);

        for consumer in consumers {
            let mut rx = shutdown_tx.subscribe();
            let task = tokio::spawn(async move {
                supervise_one(consumer, &mut rx).await;
            });
            handles.push(task);
        }

        // Relay supervisor: drain reactor outbox into the log. The
        // engine's aggregator registry is folded after every successful
        // append so reactor-emitted events become visible via
        // `engine.snapshot()` — without this, only caller-emitted
        // events update the engine-level state.
        let relay = RelayLoop::new(log.clone(), outbox.clone())
            .with_engine_aggregators(aggregators.clone());
        let mut relay_rx = shutdown_tx.subscribe();
        let relay_task = tokio::spawn(async move {
            supervise_relay(relay, &mut relay_rx).await;
        });
        handles.push(relay_task);

        Self {
            log, checkpoint, outbox, shutdown_tx, handles,
            default_metadata,
            aggregators, consumer_ids, observer,
        }
    }

    /// Emit one or more Facts to the log.
    ///
    /// Returns an [`EmitBuilder`] — chain `.metadata()`,
    /// `.correlation_id()`, `.parent_id()` and finally `.await` to run
    /// the write. `.await` returns once facts are durably in the log;
    /// the reactor chain runs asynchronously after that. Use
    /// `.settled().await` to also wait for the chain to drain.
    ///
    /// ```ignore
    /// // simplest — durable append, returns immediately
    /// engine.emit(fact).await?;
    /// // command-handler envelope — propagate trigger correlation
    /// engine.emit(out)
    ///     .correlation_id(trigger_corr)
    ///     .parent_id(trigger_event_id)
    ///     .await?;
    /// // batch
    /// engine.emit(vec![f1, f2]).await?;
    /// // wait for the whole causal chain (tests, sync handlers)
    /// engine.emit(fact).settled().await?;
    /// ```
    pub fn emit<I: Into<EmitInput>>(&self, input: I) -> EmitBuilder<'_> {
        EmitBuilder {
            engine: self,
            input: input.into(),
            correlation_id: None,
            parent_id: None,
            metadata: Metadata::new(),
        }
    }

    /// Hydrate an aggregate by folding its full stream from the log.
    /// Returns the aggregate state and the current stream version
    /// (informational — backend-internal cursor, not used for user-
    /// facing CAS).
    ///
    /// Stream identity comes from `F::CATEGORY` + `id`; the same
    /// convention `Engine::emit` uses on write. Caller picks both
    /// type params so the same `Aggregate` impl can fold different
    /// Fact streams (e.g. `load::<PipelineState, ScrapeEvent>`).
    pub async fn load<A, F>(
        &self,
        id: Uuid,
    ) -> Result<(A, StreamVersion)>
    where
        A: Aggregate + Apply<F>,
        F: Fact + DeserializeOwned,
    {
        let events = self.log.load_stream(F::CATEGORY, id, None).await?;
        let mut agg = A::default();
        let mut version = StreamVersion::ZERO;
        for event in events {
            let fact: F = serde_json::from_value(event.payload)?;
            agg.apply(&fact);
            if let Some(v) = event.version {
                version = v;
            }
        }
        Ok((agg, version))
    }

    async fn execute_emit(&self, b: EmitBuilder<'_>) -> Result<EmitResult> {
        // Empty batch is a successful no-op. Returns the log's
        // current latest position so a downstream `settled()`
        // waits for any pre-existing pending work to drain (rather
        // than returning trivially against `LogCursor::ZERO`).
        if b.input.facts.is_empty() {
            let position = self.log.latest_position().await?;
            let correlation_id = b.correlation_id.unwrap_or_else(Uuid::new_v4);
            return Ok(EmitResult { position, correlation_id });
        }

        let correlation = b.correlation_id.unwrap_or_else(Uuid::new_v4);
        let mut last_position = LogCursor::ZERO;

        // Merge engine defaults under per-emit metadata. Per-emit
        // overrides on key collision; non-colliding keys merge.
        let merged_metadata = {
            let mut m = self.default_metadata.clone();
            for (k, v) in b.metadata.iter() {
                m.insert(k.clone(), v.clone());
            }
            m
        };

        for fact in b.input.facts {
            let event_type = format!("{}:{}", fact.category(), fact.variant_name());
            let occurred_at = fact.occurred_at().unwrap_or_else(Utc::now);
            let stream_id = fact.stream_id();
            let payload = fact.to_value()?;
            let new_event = NewEvent {
                event_id:        Uuid::new_v4(),
                parent_id:       b.parent_id,
                correlation_id:  correlation,
                event_type,
                payload,
                created_at:      occurred_at,
                aggregate_type:  Some(fact.category().to_string()),
                aggregate_id:    Some(stream_id),
                metadata:        merged_metadata.clone(),
                ephemeral:       None,
                persistent:      true,
            };

            // Capture for engine-level aggregator fold before the log
            // write consumes new_event.
            let agg_event_type = new_event.event_type.clone();
            let agg_payload = new_event.payload.clone();

            let event_id = new_event.event_id;
            let result = self.log.append(new_event).await?;
            last_position = result.position;

            // Mirror into the engine-level registry so out-of-band
            // `engine.snapshot::<A>(stream_id)` reads stay fresh.
            // Consumers maintain their OWN registry clones for
            // in-body `ctx.aggregate` reads — independent state.
            if let Some(reg) = &self.aggregators {
                let snapshots = reg.apply_event(&agg_event_type, &agg_payload);
                if let Some(obs) = self.observer.as_ref() {
                    reg.notify_observer(
                        &snapshots,
                        obs.as_ref(),
                        correlation,
                        result.position,
                        event_id,
                    );
                }
            }
        }
        Ok(EmitResult {
            position: last_position,
            correlation_id: correlation,
        })
    }

    /// Inspect the current folded state of aggregate `A` for the
    /// given `stream_id`. Returns `None` if no aggregator is
    /// registered for `A`, or if no facts have ever been folded
    /// for this `stream_id`.
    ///
    /// ## Not for decisions
    ///
    /// **Do not** use this to read aggregate state and then `emit`
    /// based on what you saw — the value is stale by the time you
    /// hold it (other emits, reactor chains, or restarts may have
    /// moved past it), and decisions made outside the causal chain
    /// are not recorded with provenance. Make decisions **inside**
    /// reactors via `ctx.aggregate::<A>(id)`; expose query results
    /// to clients via projections (Postgres tables, etc.) keyed on
    /// `correlation_id` or the entity id.
    ///
    /// `snapshot` exists for tests, debugging, and operational
    /// inspection — assertions, status dashboards, ad-hoc CLI
    /// dumps. Treat it as a peek into in-memory state, nothing more.
    pub fn snapshot<A>(&self, stream_id: Uuid) -> Option<A>
    where
        A: crate::aggregate_v3::Aggregate + Clone,
    {
        let reg = self.aggregators.as_ref()?;
        let key = format!("{}:{}", <A as crate::aggregate_v3::Aggregate>::NAME, stream_id);
        if !reg.has_state(&key) {
            return None;
        }
        let (_, curr) = reg.get_transition_arc::<A>(stream_id);
        Some((*curr).clone())
    }

    /// Wait until every reactor chain triggered by `emit_result`
    /// has fully quiesced — every consumer caught up, every reactor
    /// output drained through the outbox to the log, no pending
    /// work remaining.
    ///
    /// Algorithm:
    ///
    ///   1. Read `latest = log.latest_position()`.
    ///   2. Wait for every consumer cursor to reach `latest`.
    ///   3. Wait for the outbox to drain (`outbox_pending().len()
    ///      == 0`).
    ///   4. Re-read latest. If unchanged from step 1, the chain has
    ///      quiesced — return Ok. Otherwise loop (new events
    ///      appeared while waiting).
    ///
    /// This terminates for well-formed reactor topologies because
    /// each input event produces a bounded number of outputs; the
    /// outbox eventually empties; consumers eventually catch up;
    /// no new events can appear. Self-feedback reactors (a reactor
    /// whose output triggers itself) are NOT well-formed under
    /// v0.4 (see [`Reactor`] doc) and will loop forever here too.
    ///
    /// **Semantic vs. legacy v0.3 `.settled()`**: same end-state
    /// (the full causal chain has run), different mechanism. v0.3
    /// ran reactors inline in the emitting task; v0.4 runs them
    /// asynchronously in supervisor tasks and `settle` polls their
    /// cursors. Bounded latency depends on consumer batch size +
    /// supervisor poll interval.
    pub async fn settle(&self, _result: EmitResult) -> Result<()> {
        loop {
            let p1 = self.log.latest_position().await?;

            // Wait for every consumer to catch up to p1.
            for id in &self.consumer_ids {
                self.await_observed_by(id, p1).await?;
            }

            // Wait for the relay to drain everything consumers
            // produced while we were waiting. Bounded by relay's
            // poll interval; one sleep usually enough.
            while !self.outbox.outbox_pending(1).await?.is_empty() {
                tokio::time::sleep(POLL_INTERVAL).await;
            }

            // If no new events landed in the log during the above
            // waits, the chain has quiesced. Otherwise loop.
            let p2 = self.log.latest_position().await?;
            if p1 == p2 { return Ok(()); }
        }
    }

    /// Block until consumer `id` has caught up to `pos`. Polls the
    /// engine's internal CheckpointStore. A runtime that wires in
    /// LISTEN/NOTIFY or similar can override this later.
    pub async fn await_observed_by(
        &self,
        id: &str,
        pos: LogCursor,
    ) -> Result<()> {
        loop {
            let cursor = self.checkpoint.get(id).await?;
            if let Some(c) = cursor {
                if c >= pos { return Ok(()); }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Signal shutdown; drain in-flight consumer steps; halt.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        for handle in self.handles {
            let _ = handle.await;
        }
        Ok(())
    }
}

async fn supervise_one(
    consumer: Arc<dyn Supervisable>,
    shutdown: &mut broadcast::Receiver<()>,
) {
    loop {
        if shutdown.try_recv().is_ok() { break; }

        // Catch panics from the consumer body so a misconfiguration
        // (e.g., ctx.aggregate without aggregators registered) or a
        // downstream-library panic doesn't kill the spawned tokio task
        // silently — which would leave the consumer permanently dead
        // while the engine keeps running. Panics become an ERROR-level
        // log + backoff retry, mirroring the existing Err(_) recovery.
        let stepped = AssertUnwindSafe(consumer.step(SUPERVISOR_BATCH))
            .catch_unwind()
            .await;

        match stepped {
            Ok(Ok(StepOutcome::Progressed { .. })) => continue,
            Ok(Ok(StepOutcome::Idle)) | Ok(Ok(StepOutcome::WaitOnDep { .. })) => {
                tokio::select! {
                    _ = shutdown.recv() => break,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    consumer = consumer.consumer_id(),
                    error = %e,
                    "supervisor step errored, backing off"
                );
                tokio::select! {
                    _ = shutdown.recv() => break,
                    _ = tokio::time::sleep(BACKOFF_ON_ERROR) => {}
                }
            }
            Err(panic_payload) => {
                tracing::error!(
                    consumer = consumer.consumer_id(),
                    panic = %panic_payload_message(&panic_payload),
                    "supervisor step PANICKED — consumer will retry. \
                     Common cause: ctx.aggregate called without aggregators \
                     registered via EngineBuilder::with_aggregators"
                );
                tokio::select! {
                    _ = shutdown.recv() => break,
                    _ = tokio::time::sleep(BACKOFF_ON_ERROR) => {}
                }
            }
        }
    }
}

/// Best-effort extraction of the panic message from `catch_unwind`'s
/// payload. Standard panics (`panic!("string")`, `panic!("{} {}", ...)`)
/// produce `&'static str` or `String` payloads; non-string payloads
/// (custom types passed to `panic_any`) fall through to a placeholder.
fn panic_payload_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

async fn supervise_relay(
    relay: RelayLoop,
    shutdown: &mut broadcast::Receiver<()>,
) {
    loop {
        if shutdown.try_recv().is_ok() {
            // One final drain before halting so in-flight reactor
            // outputs reach the log on clean shutdown.
            let _ = relay.drain_once(RELAY_BATCH).await;
            break;
        }
        match relay.drain_once(RELAY_BATCH).await {
            Ok(0) => {
                tokio::select! {
                    _ = shutdown.recv() => {
                        let _ = relay.drain_once(RELAY_BATCH).await;
                        break;
                    }
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "relay drain errored, backing off");
                tokio::select! {
                    _ = shutdown.recv() => break,
                    _ = tokio::time::sleep(BACKOFF_ON_ERROR) => {}
                }
            }
        }
    }
    // Suppress unused warning; ts of last drain attempt for ops.
    let _ = Utc::now();
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contexts::Ctx;
        use crate::memory_store::MemoryStore;
    use crate::reactor_v3::Events;
    use chrono::DateTime;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct UserCreated {
        user_id:     Uuid,
        occurred_at: DateTime<Utc>,
    }
    impl Fact for UserCreated {
        const CATEGORY: &'static str = "user";
        fn name(&self) -> &str { "user_created" }
        fn stream_id(&self) -> Uuid { self.user_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct WelcomeQueued {
        user_id: Uuid,
    }
    impl Fact for WelcomeQueued {
        const CATEGORY: &'static str = "welcome";
        fn name(&self) -> &str { "welcome_queued" }
        fn stream_id(&self) -> Uuid { self.user_id }
    }

    /// Projector that records every user_id it sees.
    #[derive(Default, Clone)]
    struct UserRoster {
        seen: Arc<parking_lot::Mutex<Vec<Uuid>>>,
    }
    #[async_trait]
    impl Projector for UserRoster {
        type Fact = UserCreated;
        const GROUP_NAME: &'static str = "users";
        async fn project(
            &self, fact: &UserCreated, _ctx: Ctx<'_>,
        ) -> Result<()> {
            self.seen.lock().push(fact.user_id);
            Ok(())
        }
    }

    /// Reactor that emits WelcomeQueued for each UserCreated.
    struct WelcomeReactor;
    #[async_trait]
    impl Reactor for WelcomeReactor {
        type Trigger = UserCreated;
        const GROUP_NAME: &'static str = "welcome.reactor";
        async fn react(
            &self, trigger: &UserCreated, _ctx: Ctx<'_>,
        ) -> Result<Events> {
            let mut out = Events::new();
            out.push(WelcomeQueued { user_id: trigger.user_id });
            Ok(out)
        }
    }

    fn store() -> Arc<MemoryStore> { Arc::new(MemoryStore::new()) }

    #[tokio::test]
    async fn engine_drives_projector_end_to_end() {
        let store = store();
        let roster = UserRoster::default();
        let seen = roster.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(roster)
        .build();

        // Emit 3 facts.
        for _ in 0..3 {
            engine.emit(UserCreated {
                user_id:     Uuid::new_v4(),
                occurred_at: Utc::now(),
            }).await.unwrap();
        }

        // Wait for the projector to catch up. Brittle but bounded.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if seen.lock().len() == 3 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "projector did not catch up within 3s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(seen.lock().len(), 3);

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_drives_reactor_with_relay_drain_end_to_end() {
        let store = store();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_assertion = counter.clone();

        // A second projector that observes the WelcomeQueued facts
        // emitted by the reactor — verifies the full chain:
        //   emit UserCreated → reactor emits WelcomeQueued → relay
        //   drains to log → second projector sees WelcomeQueued.
        struct WelcomeCounter(Arc<AtomicUsize>);
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct WelcomeQueuedFact { user_id: Uuid }
        impl Fact for WelcomeQueuedFact {
            const CATEGORY: &'static str = "welcome";
            fn name(&self) -> &str { "welcome_queued" }
            fn stream_id(&self) -> Uuid { self.user_id }
        }
        #[async_trait]
        impl Projector for WelcomeCounter {
            type Fact = WelcomeQueuedFact;
            const GROUP_NAME: &'static str = "welcome.counter";
            async fn project(
                &self, _fact: &WelcomeQueuedFact, _ctx: Ctx<'_>,
            ) -> Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_reactor(WelcomeReactor)
        .with_projector(WelcomeCounter(counter))
        .build();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        // Wait for the reactor → relay → projector chain.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if counter_for_assertion.load(Ordering::SeqCst) == 1 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "chain did not complete within 3s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(counter_for_assertion.load(Ordering::SeqCst), 1);

        engine.shutdown().await.unwrap();
    }

    /// Regression test for the RelayLoop → engine-aggregator fold path.
    ///
    /// Before this path existed, `engine.snapshot::<A>(stream_id)`
    /// reflected only caller-emitted facts; reactor-emitted facts
    /// updated each consumer's private registry clone but were
    /// invisible to out-of-band readers. That made the saga-style
    /// "one aggregate, many fact types" pattern unusable from tests —
    /// you could emit a trigger and verify its direct effect on state,
    /// but not the effect of downstream reactor outputs.
    ///
    /// The fix attaches the engine aggregator registry to RelayLoop so
    /// every drained outbox row folds into it after `log.append`. This
    /// test pins that contract: emit a UserCreated, the reactor emits
    /// a WelcomeQueued, settle, then `engine.snapshot::<ChainCount>` on
    /// the user_id stream reflects BOTH (one UserCreated, one
    /// WelcomeQueued — total = 2).
    ///
    /// If a future refactor of RelayLoop omits the `apply_event` call
    /// (or wires it to the wrong registry), this assertion drops to 1.
    #[tokio::test]
    async fn engine_snapshot_sees_reactor_emitted_facts() {
        use crate::aggregate_v3::{Aggregate, Apply};

        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct ChainCount { applied: u32 }
        impl Aggregate for ChainCount {
            const NAME: &'static str = "ChainCount";
        }
        impl Apply<UserCreated> for ChainCount {
            fn apply(&mut self, _: &UserCreated) { self.applied += 1; }
        }
        impl Apply<WelcomeQueued> for ChainCount {
            fn apply(&mut self, _: &WelcomeQueued) { self.applied += 1; }
        }

        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([
            Aggregator::for_type::<ChainCount, UserCreated>(),
            Aggregator::for_type::<ChainCount, WelcomeQueued>(),
        ])
        .with_reactor(WelcomeReactor)
        .build();

        let user_id = Uuid::new_v4();
        engine.emit(UserCreated {
            user_id,
            occurred_at: Utc::now(),
        }).settled().await.unwrap();

        let state: ChainCount = engine
            .snapshot::<ChainCount>(user_id)
            .expect("snapshot must exist after settled emit");
        assert_eq!(state.applied, 2,
            "engine.snapshot must reflect BOTH caller-emitted UserCreated \
             AND reactor-emitted WelcomeQueued — relay-side fold contract");

        engine.shutdown().await.unwrap();
    }

    /// Pin the `Aggregator::for_type_with_id_fn` contract: a single
    /// fact type can register two aggregators with different keys.
    ///
    /// Before 0.4.5: the `#[aggregator(id_fn = "...")]` macro accepted
    /// the attribute but the factory hard-coded `Fact::stream_id`, so
    /// every aggregator registered for the same fact type folded into
    /// the same key. The "per-signal aggregate vs per-run aggregate"
    /// pattern was impossible without emitting twin events (a smell
    /// that surfaced in the scout `SignalEvent::ReviewCompleted` twin
    /// of `SystemEvent::ReviewVerdictReached`).
    ///
    /// This test proves the bridge: one `UserCreated` fact, two
    /// aggregators — one keyed by `user_id` (default via `for_type`),
    /// one keyed by a *different* `org_id` field via
    /// `for_type_with_id_fn`. Both fold; neither's state bleeds into
    /// the other's key.
    #[tokio::test]
    async fn aggregator_for_type_with_id_fn_keys_independently() {
        use crate::aggregate_v3::{Aggregate, Apply};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct OrgUserCreated {
            user_id: Uuid,
            org_id: Uuid,
            occurred_at: DateTime<Utc>,
        }
        impl Fact for OrgUserCreated {
            const CATEGORY: &'static str = "org_user";
            fn name(&self) -> &str { "org_user_created" }
            fn stream_id(&self) -> Uuid { self.user_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct UserCount { n: u32 }
        impl Aggregate for UserCount {
            const NAME: &'static str = "UserCount";
        }
        impl Apply<OrgUserCreated> for UserCount {
            fn apply(&mut self, _: &OrgUserCreated) { self.n += 1; }
        }

        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct OrgCount { n: u32 }
        impl Aggregate for OrgCount {
            const NAME: &'static str = "OrgCount";
        }
        impl Apply<OrgUserCreated> for OrgCount {
            fn apply(&mut self, _: &OrgUserCreated) { self.n += 1; }
        }

        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([
            // UserCount keys by the natural stream_id (user_id).
            Aggregator::for_type::<UserCount, OrgUserCreated>(),
            // OrgCount keys by org_id — a different field. Pre-0.4.5
            // this was impossible; the factory ignored id_fn and
            // also folded into user_id, collapsing both aggregators
            // onto the same key.
            Aggregator::for_type_with_id_fn::<OrgCount, OrgUserCreated, _>(
                |e: &OrgUserCreated| Some(e.org_id)
            ),
        ])
        .build();

        let org_a = Uuid::new_v4();
        let org_b = Uuid::new_v4();
        let user_1 = Uuid::new_v4();
        let user_2 = Uuid::new_v4();
        let user_3 = Uuid::new_v4();

        // Two events in org_a (user_1, user_2), one in org_b (user_3).
        engine.emit(OrgUserCreated { user_id: user_1, org_id: org_a, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(OrgUserCreated { user_id: user_2, org_id: org_a, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(OrgUserCreated { user_id: user_3, org_id: org_b, occurred_at: Utc::now() }).settled().await.unwrap();

        // UserCount: each user_id folds independently → 1, 1, 1.
        assert_eq!(engine.snapshot::<UserCount>(user_1).unwrap().n, 1);
        assert_eq!(engine.snapshot::<UserCount>(user_2).unwrap().n, 1);
        assert_eq!(engine.snapshot::<UserCount>(user_3).unwrap().n, 1);

        // OrgCount: keyed by org_id → 2 for org_a, 1 for org_b.
        assert_eq!(engine.snapshot::<OrgCount>(org_a).unwrap().n, 2,
            "id_fn must extract org_id, not user_id (Fact::stream_id)");
        assert_eq!(engine.snapshot::<OrgCount>(org_b).unwrap().n, 1);

        // Cross-check: org_a snapshot at user_1's key must be None
        // (the OrgCount aggregator never folded there).
        assert!(engine.snapshot::<OrgCount>(user_1).is_none(),
            "OrgCount keyed by org_id must not appear under user_id key");

        engine.shutdown().await.unwrap();
    }

    /// `id_fn` that returns `Option<Uuid>` and yields `None` for some
    /// facts must skip the fold entirely (not fold at `Uuid::nil`).
    #[tokio::test]
    async fn aggregator_id_fn_returning_none_skips_fold() {
        use crate::aggregate_v3::{Aggregate, Apply};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct MaybeRunEvent {
            stream_id: Uuid,
            run_id: Option<Uuid>,
            occurred_at: DateTime<Utc>,
        }
        impl Fact for MaybeRunEvent {
            const CATEGORY: &'static str = "maybe_run";
            fn name(&self) -> &str { "maybe_run_event" }
            fn stream_id(&self) -> Uuid { self.stream_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct RunCounter { n: u32 }
        impl Aggregate for RunCounter {
            const NAME: &'static str = "RunCounter";
        }
        impl Apply<MaybeRunEvent> for RunCounter {
            fn apply(&mut self, _: &MaybeRunEvent) { self.n += 1; }
        }

        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([
            Aggregator::for_type_with_id_fn::<RunCounter, MaybeRunEvent, _>(
                |e: &MaybeRunEvent| e.run_id
            ),
        ])
        .build();

        let run = Uuid::new_v4();

        // Two events with run_id, one without.
        engine.emit(MaybeRunEvent { stream_id: Uuid::new_v4(), run_id: Some(run), occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(MaybeRunEvent { stream_id: Uuid::new_v4(), run_id: None, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(MaybeRunEvent { stream_id: Uuid::new_v4(), run_id: Some(run), occurred_at: Utc::now() }).settled().await.unwrap();

        assert_eq!(engine.snapshot::<RunCounter>(run).unwrap().n, 2,
            "only the two facts with run_id Some should fold");

        // The None-run fact must not have created an entry at
        // Uuid::nil — verify nothing leaked there.
        assert!(engine.snapshot::<RunCounter>(Uuid::nil()).is_none(),
            "id_fn returning None must skip the fold entirely, not fold at nil");

        engine.shutdown().await.unwrap();
    }

    // Types for the macro id_fn regression test below — kept at
    // module scope (not inside a test fn) so the `#[aggregators]`
    // module macro can emit at proper Rust scope.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TaggedEvent {
        event_id: Uuid,
        tag_id: Uuid,
        occurred_at: DateTime<Utc>,
    }
    impl Fact for TaggedEvent {
        const CATEGORY: &'static str = "tagged";
        fn name(&self) -> &str { "tagged" }
        // stream_id intentionally NOT tag_id — proves the macro
        // uses id_fn over stream_id.
        fn stream_id(&self) -> Uuid { self.event_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }
    impl TaggedEvent {
        fn tag(&self) -> Uuid { self.tag_id }
    }

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct TagBucket { count: u32 }
    impl crate::aggregate_v3::Aggregate for TagBucket {
        const NAME: &'static str = "TagBucket";
    }

    use causal_core_macros::{aggregator, aggregators};

    #[aggregators]
    mod tagged_aggs {
        use super::*;

        #[aggregator(id_fn = "tag")]
        fn on_tagged(b: &mut TagBucket, e: TaggedEvent) {
            b.count += 1;
            let _ = e;
        }
    }

    /// End-to-end macro test: `#[aggregator(id_fn = "method")]` must
    /// emit a factory that keys by the user method's return value, not
    /// by `Fact::stream_id`. This is the contract the scout side
    /// depends on (e.g. SignalLifecycle keyed by signal_id from a
    /// CuriosityEvent whose stream_id is nil).
    ///
    /// Regression test for the v0.4.0–0.4.4 bug where the macro
    /// accepted the attribute but the factory hard-coded
    /// `Fact::stream_id`.
    #[tokio::test]
    async fn macro_aggregator_id_fn_actually_keys_by_method() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators(tagged_aggs::aggregators())
        .build();

        let tag_a = Uuid::new_v4();
        let tag_b = Uuid::new_v4();

        engine.emit(TaggedEvent { event_id: Uuid::new_v4(), tag_id: tag_a, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(TaggedEvent { event_id: Uuid::new_v4(), tag_id: tag_a, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(TaggedEvent { event_id: Uuid::new_v4(), tag_id: tag_b, occurred_at: Utc::now() }).settled().await.unwrap();

        assert_eq!(engine.snapshot::<TagBucket>(tag_a).unwrap().count, 2,
            "#[aggregator(id_fn = \"tag\")] must key by tag_id, not event_id");
        assert_eq!(engine.snapshot::<TagBucket>(tag_b).unwrap().count, 1);

        engine.shutdown().await.unwrap();
    }

    // ── Phase 5a — Aggregate-stream emit gating ──

    /// Aggregate registered for stream-policy testing.
    #[derive(Default, Clone, Serialize, Deserialize)]
    struct UserAgg;
    impl crate::aggregate_v3::Aggregate for UserAgg {
        const NAME: &'static str = "UserAgg";
    }
    impl crate::aggregate_v3::Apply<UserCreated> for UserAgg {
        fn apply(&mut self, _fact: &UserCreated) {}
    }

    #[tokio::test]
    async fn emit_to_unregistered_stream_succeeds_by_default() {
        // Unregistered category defaults to OpenAppend for backward
        // compat with the legacy engine. Phase 5a does NOT make all
        // streams require explicit registration.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_to_aggregate_stream_succeeds_without_ceremony() {
        // OCC is no longer part of the user-facing API. Emitting a
        // fact whose CATEGORY has an aggregator registered just
        // appends + folds — no version check, no error, no special
        // builder methods required.
        //
        // The aggregator still folds in-memory state from each fact
        // (verified separately by aggregate-state tests); registration
        // does not change the emit path.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .build();

        let result = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await;

        assert!(result.is_ok(),
                "emit to aggregate stream succeeds without .expecting(): {:?}",
                result.err());
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registering_aggregator_is_invisible_to_emit_path() {
        // Aggregator registration is read-side only — it does not
        // change emit semantics. Both the registered and unregistered
        // streams accept naked emit identically.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .build();

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct OrderPlaced { order_id: Uuid, occurred_at: DateTime<Utc> }
        impl Fact for OrderPlaced {
            const CATEGORY: &'static str = "order";
            fn name(&self) -> &str { "order_placed" }
            fn stream_id(&self) -> Uuid { self.order_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        engine.emit(OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        // Registered category emits identically.
        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        engine.shutdown().await.unwrap();
    }

    // ── Phase 5b — load + aggregator fold ──

    /// Counter aggregate for OCC tests.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    enum CounterFact {
        Inc { by: i32, occurred_at: DateTime<Utc>, counter_id: Uuid },
        Reset { occurred_at: DateTime<Utc>, counter_id: Uuid },
    }
    impl Fact for CounterFact {
        const CATEGORY: &'static str = "counter";
        fn name(&self) -> &str {
            match self {
                CounterFact::Inc { .. }   => "inc",
                CounterFact::Reset { .. } => "reset",
            }
        }
        fn stream_id(&self) -> Uuid {
            match self {
                CounterFact::Inc { counter_id, .. }
              | CounterFact::Reset { counter_id, .. } => *counter_id,
            }
        }
        fn occurred_at(&self) -> Option<DateTime<Utc>> {
            Some(match self {
                CounterFact::Inc { occurred_at, .. }
              | CounterFact::Reset { occurred_at, .. } => *occurred_at,
            })
        }
    }

    #[derive(Default, Debug, PartialEq, Clone, Serialize, Deserialize)]
    struct Counter { value: i32 }
    impl crate::aggregate_v3::Aggregate for Counter {
        const NAME: &'static str = "Counter";
    }
    impl crate::aggregate_v3::Apply<CounterFact> for Counter {
        fn apply(&mut self, fact: &CounterFact) {
            match fact {
                CounterFact::Inc { by, .. } => self.value += by,
                CounterFact::Reset { .. }   => self.value = 0,
            }
        }
    }

    #[tokio::test]
    async fn load_returns_default_for_unknown_aggregate() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build();

        let id = Uuid::new_v4();
        let (agg, ver) = engine.load::<Counter, CounterFact>(id).await.unwrap();
        assert_eq!(agg, Counter::default());
        assert_eq!(ver, StreamVersion::ZERO);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_batch_round_trips_through_aggregator_fold() {
        // Emitting a batch of facts of the registered Fact type
        // folds them into the engine's aggregator state, readable
        // via the read-side hydration helper.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build();

        let id = Uuid::new_v4();
        engine.emit(vec![
            CounterFact::Inc { by: 3, occurred_at: Utc::now(), counter_id: id },
            CounterFact::Inc { by: 5, occurred_at: Utc::now(), counter_id: id },
        ]).await.unwrap();

        let (agg, _) = engine.load::<Counter, CounterFact>(id).await.unwrap();
        assert_eq!(agg.value, 8);

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn append_uses_facts_occurred_at_for_persisted_created_at() {
        // C7: occurred_at on the fact is the canonical clock; backends
        // persist it as created_at so replay reproduces.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build();

        let id = Uuid::new_v4();
        let pinned = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap().with_timezone(&Utc);
        engine.emit(vec![
            CounterFact::Inc { by: 1, occurred_at: pinned, counter_id: id },
        ]).await.unwrap();

        let events = EventLogBackend::load_stream(
            store.as_ref(), <CounterFact as Fact>::CATEGORY, id, None,
        ).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].created_at, pinned,
                   "persisted created_at == fact.occurred_at()");

        engine.shutdown().await.unwrap();
    }

    // ── Phase 7 — MultiProjector engine integration ──

    #[tokio::test]
    async fn engine_drives_multi_projector_seeing_heterogeneous_events() {
        use crate::multi_projector::MultiProjector;

        #[derive(Default, Clone)]
        struct AuditAll {
            seen: Arc<parking_lot::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl MultiProjector for AuditAll {
            const GROUP_NAME: &'static str = "audit";
            const CATEGORIES: &'static [&'static str] = &["alpha", "beta"];

            async fn project(
                &self,
                event: &crate::types::PersistedEvent,
                _ctx: Ctx<'_>,
            ) -> Result<()> {
                self.seen.lock().push(event.event_type.clone());
                Ok(())
            }
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct A { a_id: Uuid, occurred_at: DateTime<Utc> }
        impl Fact for A {
            const CATEGORY: &'static str = "alpha";
            fn name(&self) -> &str { "a" }
            fn stream_id(&self) -> Uuid { self.a_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct B { b_id: Uuid, occurred_at: DateTime<Utc> }
        impl Fact for B {
            const CATEGORY: &'static str = "beta";
            fn name(&self) -> &str { "b" }
            fn stream_id(&self) -> Uuid { self.b_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        let store = store();
        let auditor = AuditAll::default();
        let seen = auditor.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_multi_projector(auditor)
        .build();

        engine.emit(A { a_id: Uuid::new_v4(), occurred_at: Utc::now() }).await.unwrap();
        engine.emit(B { b_id: Uuid::new_v4(), occurred_at: Utc::now() }).await.unwrap();
        engine.emit(A { a_id: Uuid::new_v4(), occurred_at: Utc::now() }).await.unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if seen.lock().len() == 3 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "multi_projector did not see all 3 events within 3s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let names = seen.lock().clone();
        assert_eq!(names, vec!["alpha:a", "beta:b", "alpha:a"]);

        engine.shutdown().await.unwrap();
    }

    // ── 0.3.1 fixes — caller-supplied envelope fields ──

    #[tokio::test]
    async fn emit_with_correlation_id_stamps_envelope() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();

        let cmd_correlation = Uuid::new_v4();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .correlation_id(cmd_correlation)
        .await.unwrap();

        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].correlation_id, cmd_correlation,
                   "persisted correlation_id MUST match caller-supplied id");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_with_parent_and_metadata_stamps_envelope() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();

        let parent = Uuid::new_v4();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .parent_id(parent)
        .metadata("_run_id", "run-abc")
        .metadata("_schema_v", 2)
        .await.unwrap();

        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events[0].parent_id, Some(parent));
        assert_eq!(
            events[0].metadata.get("_run_id").and_then(|v| v.as_str()),
            Some("run-abc"),
        );
        assert_eq!(
            events[0].metadata.get("_schema_v").and_then(|v| v.as_i64()),
            Some(2),
        );

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_batch_propagates_correlation_id_to_every_fact() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build();

        let id = Uuid::new_v4();
        let cmd_correlation = Uuid::new_v4();

        engine.emit(vec![
            CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
            CounterFact::Inc { by: 2, occurred_at: Utc::now(), counter_id: id },
        ])
        .correlation_id(cmd_correlation)
        .await.unwrap();

        let events = EventLogBackend::load_stream(
            store.as_ref(), <CounterFact as Fact>::CATEGORY, id, None,
        ).await.unwrap();
        assert_eq!(events.len(), 2);
        for ev in &events {
            assert_eq!(ev.correlation_id, cmd_correlation,
                       "every fact in the batch carries the caller's correlation_id");
        }

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn await_observed_by_does_not_require_caller_to_pass_checkpoint() {
        // The caller should not need to pass an Arc<dyn CheckpointStore>
        // — the engine already owns one internally.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(UserRoster::default())
        .build();

        let pos = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap().position;

        // 0.3.1 signature: no checkpoint param.
        engine.await_observed_by("users", pos).await.unwrap();

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registering_one_aggregate_against_two_facts_is_allowed() {
        // Multi-Fact aggregates: same A::NAME registered with two
        // distinct Apply<F> impls is legitimate (e.g. PipelineState
        // folding ScrapeEvent + LifecycleEvent). The collision check
        // distinguishes "same NAME + same Fact CATEGORY" (panic)
        // from "same NAME + different Fact CATEGORYs" (allowed).

        #[derive(Default, Debug, Clone, Serialize, Deserialize)]
        struct Multi { hits: u32 }
        impl crate::aggregate_v3::Aggregate for Multi {
            const NAME: &'static str = "Multi";
        }
        impl crate::aggregate_v3::Apply<Tick> for Multi {
            fn apply(&mut self, _t: &Tick) { self.hits += 1; }
        }
        // Second Fact type for the same Aggregate.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct Pong { pong_id: Uuid, occurred_at: DateTime<Utc> }
        impl Fact for Pong {
            const CATEGORY: &'static str = "pong";
            fn name(&self) -> &str { "pong" }
            fn stream_id(&self) -> Uuid { self.pong_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }
        impl crate::aggregate_v3::Apply<Pong> for Multi {
            fn apply(&mut self, _p: &Pong) { self.hits += 1; }
        }

        let store = store();
        let _engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators(vec![
            crate::aggregator::Aggregator::for_type::<Multi, Tick>(),
            crate::aggregator::Aggregator::for_type::<Multi, Pong>(),
        ])
        .build();
        // No panic — different Fact CATEGORYs.
    }

    #[tokio::test]
    #[should_panic(expected = "duplicate Aggregate::NAME `TickCounter`")]
    async fn registering_two_aggregators_with_same_name_panics() {
        // Two aggregators with the same Aggregate::NAME would collide
        // on the registry key `{NAME}:{id}` and silently overwrite
        // each other's state. EngineBuilder catches this at
        // registration time.
        //
        // Pattern: legacy `Aggregator::new(...)` returned non-OCC
        // aggregators that pre-v0.4 tests register two of. Under
        // v0.4 both would need distinct A::NAME consts. The check
        // protects users from naming both `"TickCounter"` by mistake.
        let store = store();
        let _engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators(vec![
            crate::aggregator::Aggregator::for_type::<TickCounter, Tick>(),
            crate::aggregator::Aggregator::for_type::<TickCounter, Tick>(),
            // ↑ same NAME twice — panic here.
        ]);
    }

    #[tokio::test]
    #[should_panic(expected = "duplicate GROUP_NAME `users`")]
    async fn registering_two_consumers_with_same_group_name_panics() {
        // Two UserRoster instances would share a cursor key — silent
        // corruption. EngineBuilder catches this at registration time.
        let store = store();
        let _engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(UserRoster::default())
        .with_projector(UserRoster::default()); // <- panic here
    }

    #[tokio::test]
    async fn with_projectors_registers_a_vec_of_boxed_projectors() {
        // The macro-generated `module::projectors() -> Vec<Box<dyn
        // ProjectorRegistration>>` shape. Two heterogeneous projector
        // types, registered in one go.
        #[derive(Default)]
        struct A { hit: Arc<AtomicUsize> }
        #[async_trait]
        impl Projector for A {
            type Fact = UserCreated;
            const GROUP_NAME: &'static str = "bulk.a";
            async fn project(&self, _f: &UserCreated, _: Ctx<'_>) -> Result<()> {
                self.hit.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        #[derive(Default)]
        struct B { hit: Arc<AtomicUsize> }
        #[async_trait]
        impl Projector for B {
            type Fact = UserCreated;
            const GROUP_NAME: &'static str = "bulk.b";
            async fn project(&self, _f: &UserCreated, _: Ctx<'_>) -> Result<()> {
                self.hit.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let a = A::default();
        let b = B::default();
        let a_hit = a.hit.clone();
        let b_hit = b.hit.clone();

        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projectors(vec![
            Box::new(a) as Box<dyn ProjectorRegistration>,
            Box::new(b) as Box<dyn ProjectorRegistration>,
        ])
        .build();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if a_hit.load(Ordering::SeqCst) >= 1 && b_hit.load(Ordering::SeqCst) >= 1 {
                break;
            }
            assert!(std::time::Instant::now() < deadline,
                    "both bulk-registered projectors didn't see the event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_snapshot_reads_state_folded_on_emit() {
        // Inspection path: emit Ticks for one stream_id, then read
        // TickCounter via engine.snapshot::<A>(id) without going
        // through a consumer.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators(vec![tick_aggregator()])
        .build();

        // Tick.stream_id() returns Uuid::nil() — all ticks fold into
        // the nil-keyed slot.
        for i in 0..5 {
            engine.emit(Tick { seq: i, occurred_at: Utc::now() }).await.unwrap();
        }

        let counter = engine.snapshot::<TickCounter>(Uuid::nil())
            .expect("snapshot Some after 5 ticks folded");
        assert_eq!(counter.count, 5,
                   "engine-level registry folded all 5 emitted Ticks");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_snapshot_returns_none_without_aggregators() {
        // No aggregators registered → snapshot returns None
        // (no panic, no implicit registration).
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();
        assert!(engine.snapshot::<TickCounter>(Uuid::nil()).is_none());
    }

    #[tokio::test]
    async fn engine_on_dlq_mapper_drains_synthesized_fact_to_log() {
        // End-to-end: register a reactor that always fails on
        // UserCreated. Configure on_dlq to synthesize HandlerFailed.
        // After settle, the log contains the synthesized fact.

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct HandlerFailed {
            group_name: String,
            attempts: u32,
        }
        impl Fact for HandlerFailed {
            const CATEGORY: &'static str = "ops";
            fn name(&self) -> &str { "handler_failed" }
            fn stream_id(&self) -> Uuid { Uuid::nil() }
        }

        struct AlwaysFails;
        #[async_trait]
        impl Reactor for AlwaysFails {
            type Trigger = UserCreated;
            const GROUP_NAME: &'static str = "always-fails-e2e";
            async fn react(&self, _t: &UserCreated, _: Ctx<'_>) -> Result<Events> {
                Err(anyhow!("boom"))
            }
        }

        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .on_dlq(|info: DlqInfo| Some(HandlerFailed {
            group_name: info.group_name,
            attempts: info.attempts,
        }))
        .with_max_attempts(2)
        .with_reactor(AlwaysFails)
        .build();

        let result = engine.emit(UserCreated {
            user_id: Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        // Wait until the reactor has retried + DLQ-mapped + the
        // relay has drained the synthetic fact to the log.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let events = EventLogBackend::load_from(
                store.as_ref(), LogCursor::ZERO, 10,
            ).await.unwrap();
            let dlq_emitted = events.iter()
                .any(|e| e.event_type == "ops:handler_failed");
            if dlq_emitted { break; }
            assert!(std::time::Instant::now() < deadline,
                    "DLQ-mapped fact never made it to the log");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Trigger UserCreated + synthesized HandlerFailed both present.
        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert!(events.iter().any(|e| e.event_type == "user:user_created"));
        assert!(events.iter().any(|e| e.event_type == "ops:handler_failed"));

        let _ = result;
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_settle_waits_for_reactor_chains_to_quiesce() {
        // The bug: settle waited only for direct consumers to reach
        // the emit position. Reactor outputs flowing through the
        // relay → log → downstream consumers weren't covered.
        //
        // Setup:
        //   emit UserCreated
        //   → WelcomeReactor reacts, emits WelcomeQueued (via outbox)
        //   → relay drains WelcomeQueued to log
        //   → WelcomeCounter projector observes WelcomeQueued
        //
        // After fix: settle waits until log position stabilizes AND
        // every consumer has caught up to that stable position.
        let store = store();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_c = counter.clone();

        struct WelcomeCounter(Arc<AtomicUsize>);
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct WelcomeQueuedFact { user_id: Uuid }
        impl Fact for WelcomeQueuedFact {
            const CATEGORY: &'static str = "welcome";
            fn name(&self) -> &str { "welcome_queued" }
            fn stream_id(&self) -> Uuid { self.user_id }
        }
        #[async_trait]
        impl Projector for WelcomeCounter {
            type Fact = WelcomeQueuedFact;
            const GROUP_NAME: &'static str = "settle.chain.counter";
            async fn project(
                &self, _f: &WelcomeQueuedFact, _ctx: Ctx<'_>,
            ) -> Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_reactor(WelcomeReactor)
        .with_projector(WelcomeCounter(counter_c))
        .build();

        let result = engine.emit(UserCreated {
            user_id: Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        engine.settle(result).await.unwrap();

        // Without chain-aware settle: this assert is flaky — the
        // WelcomeCounter may not have observed the reactor's
        // downstream output yet.
        // With chain-aware settle: the assert is deterministic.
        assert_eq!(counter.load(Ordering::SeqCst), 1,
                   "settle should wait until the WelcomeReactor → \
                    relay → WelcomeCounter chain quiesces");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_settle_waits_for_every_consumer_to_catch_up() {
        // After emit + settle, every registered consumer's cursor is
        // ≥ the emit position. Pin the contract.
        let store = store();
        let roster = UserRoster::default();
        let seen = roster.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(roster)
        .build();

        let result = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        engine.settle(result).await.unwrap();

        // After settle, the projector has definitely processed the
        // event — no polling sleep needed.
        assert_eq!(seen.lock().len(), 1);

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn empty_emit_followed_by_settle_waits_for_pre_existing_work() {
        // The bug: empty emit returned EmitResult { position: ZERO,
        // version: None }. settle(result) trivially returned because
        // every consumer cursor was >= ZERO — even if a previous emit
        // had pending work the consumer hadn't processed yet.
        //
        // After fix: empty emit returns the log's current latest
        // position, so settle waits for pending work to drain.
        let store = store();
        let roster = UserRoster::default();
        let seen = roster.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(roster)
        .build();

        // Emit some real work — consumer hasn't necessarily processed
        // it yet (async runner).
        let _ = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        // Now an empty emit. Returns the log's current latest
        // position, NOT ZERO.
        let empty_result = engine.emit(Vec::<UserCreated>::new()).await.unwrap();
        let latest = EventLogBackend::latest_position(store.as_ref()).await.unwrap();
        assert_eq!(empty_result.position, latest,
                   "empty emit returns log.latest_position(), not ZERO");

        // settle on the empty result waits for the pre-existing work.
        engine.settle(empty_result).await.unwrap();
        assert_eq!(seen.lock().len(), 1,
                   "settle after empty emit drained the prior UserCreated");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_settled_waits_for_consumer_to_catch_up() {
        // engine.emit(fact).settled().await is sugar over
        // emit + engine.settle(result). After it returns, every
        // registered consumer has observed the emitted fact.
        let store = store();
        let roster = UserRoster::default();
        let seen = roster.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(roster)
        .build();

        // Without .settled(), bare .await would return before the
        // async projector ran. With .settled(), we are guaranteed
        // the projector has processed the fact.
        let result = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).settled().await.unwrap();

        assert_eq!(seen.lock().len(), 1,
                   ".settled() waited for the projector to observe");
        assert_ne!(result.correlation_id, Uuid::nil(),
                   "settled still surfaces the EmitResult");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn observer_captures_reactor_execution_and_aggregate_fold() {
        // End-to-end smoke test for the ReactorObserver pipeline.
        // Registers MemoryStore as the observer; emits a fact that
        // both folds into an aggregator AND triggers a reactor;
        // asserts every observability table was populated.
        use crate::reactor_v3::{Events, Reactor};
        use std::sync::atomic::AtomicUsize;

        struct EchoReactor {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Reactor for EchoReactor {
            type Trigger = UserCreated;
            const GROUP_NAME: &'static str = "observer-echo";
            async fn react(
                &self,
                _t: &UserCreated,
                ctx: Ctx<'_>,
            ) -> Result<Events> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ctx.log(crate::types::LogLevel::Info, "echo fired");
                Ok(Events::new())
            }
            fn describe(&self, t: &UserCreated) -> Option<serde_json::Value> {
                Some(serde_json::json!({ "action": "echo", "user_id": t.user_id }))
            }
        }

        let store = store();
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_observer(store.clone())
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .with_reactor(EchoReactor { calls: calls.clone() })
        .build();

        let user_id = Uuid::new_v4();
        engine.emit(UserCreated {
            user_id,
            occurred_at: Utc::now(),
        }).settled().await.unwrap();

        // Reactor ran.
        assert!(calls.load(Ordering::SeqCst) >= 1, "reactor fired");

        // Observer hook #1: reactor_started + reactor_completed populated
        // reactor_executions for (event_id, GROUP_NAME).
        let execs = store.reactor_executions();
        assert!(
            execs.iter().any(|e| {
                let (_eid, rid) = e.key();
                rid == "observer-echo"
            }),
            "reactor_executions populated with the reactor's GROUP_NAME"
        );

        // Observer hook #2: reactor_completed pushed the attempt row.
        let attempts = store.reactor_attempt_history().lock();
        assert!(
            attempts.iter().any(|(_, rid, _, _, status, _, _, _)| {
                rid == "observer-echo" && status == "completed"
            }),
            "reactor_attempt_history has a completed row"
        );
        drop(attempts);

        // Observer hook #3: ctx.log entries drained into reactor_log_entries.
        let logs = store.reactor_log_entries().lock();
        assert!(
            logs.iter().any(|(_, rid, entry)| {
                rid == "observer-echo" && entry.message == "echo fired"
            }),
            "ctx.log() captured in reactor_log_entries"
        );
        drop(logs);

        // Observer hook #4: aggregate_folded ran from the engine-level
        // fold path AND the reactor-runner-side fold (per-consumer mirror).
        let agg_snaps = store.aggregate_state_snapshots().lock();
        assert!(
            !agg_snaps.is_empty(),
            "aggregate_state_snapshots populated by aggregate_folded"
        );
        assert!(
            agg_snaps.iter().any(|(_, _, _, key, _)| key.starts_with("UserAgg:")),
            "snapshot key is `{{NAME}}:{{id}}`"
        );
        drop(agg_snaps);

        // Observer hook #5: describe() output captured as a description snapshot.
        let descrs = store.reactor_description_snapshots().lock();
        assert!(
            descrs.iter().any(|(_, _, _, rid, descr)| {
                rid == "observer-echo"
                    && descr.get("action").and_then(|v| v.as_str()) == Some("echo")
            }),
            "describe() output captured in reactor_description_snapshots"
        );
        drop(descrs);

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_returns_none_for_id_with_no_facts() {
        // Snapshot is for inspection only. When no facts have been
        // emitted for a given stream_id, the aggregate hasn't been
        // materialized — return None rather than a fresh default
        // (which would silently fool callers asserting on existence).
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .build();

        let id = Uuid::new_v4();
        assert!(engine.snapshot::<UserAgg>(id).is_none(),
                "snapshot returns None when the aggregate has no facts yet");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_returns_some_clone_after_emit() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build();

        let id = Uuid::new_v4();
        engine.emit(CounterFact::Inc {
            by: 7, occurred_at: Utc::now(), counter_id: id,
        }).await.unwrap();

        let state = engine.snapshot::<Counter>(id)
            .expect("snapshot Some after emit folded a fact for this id");
        assert_eq!(state.value, 7);

        // Independent ids stay None.
        let other = Uuid::new_v4();
        assert!(engine.snapshot::<Counter>(other).is_none(),
                "snapshot is keyed — other ids unaffected");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_result_carries_correlation_id() {
        // EmitResult.correlation_id must surface the chain id that was
        // stamped on the emitted fact(s). Clients use it to poll
        // workflow projections; tests use it to scope `settle_to`
        // and `snapshot::<A>(stream_id)` to one run.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();

        // Auto-generated correlation when not provided.
        let r1 = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();
        assert_ne!(r1.correlation_id, Uuid::nil(),
                   "emit auto-generates correlation_id when not set");

        // Two emits without explicit correlation must produce
        // distinct correlations (each is a fresh chain).
        let r2 = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();
        assert_ne!(r1.correlation_id, r2.correlation_id,
                   "distinct emits get distinct correlation_ids");

        // Explicit correlation: result echoes what the caller set.
        let corr = Uuid::new_v4();
        let r3 = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .correlation_id(corr)
        .await.unwrap();
        assert_eq!(r3.correlation_id, corr,
                   "EmitResult.correlation_id echoes the explicit value");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_with_empty_batch_is_a_no_op() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();

        let result = engine.emit(Vec::<UserCreated>::new()).await.unwrap();
        assert_eq!(result.position, LogCursor::ZERO);
        // Empty emit still produces a fresh correlation_id (no facts
        // got stamped with it; the value is informational).
        assert_ne!(result.correlation_id, Uuid::nil());

        // No events written.
        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert!(events.is_empty());

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_default_metadata_merges_under_per_emit_metadata() {
        let store = store();
        let mut defaults = Metadata::new();
        defaults.insert("_run_id".into(), serde_json::json!("run-default"));
        defaults.insert("_actor".into(), serde_json::json!("service-a"));

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_default_metadata(defaults)
        .build();

        // Per-emit override: _run_id should win; _actor inherited; new
        // key _trace merges in.
        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .metadata("_run_id", "run-override")
        .metadata("_trace", "abc123")
        .await.unwrap();

        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let m = &events[0].metadata;
        assert_eq!(m.get("_run_id").and_then(|v| v.as_str()), Some("run-override"),
                   "per-emit overrides engine default");
        assert_eq!(m.get("_actor").and_then(|v| v.as_str()), Some("service-a"),
                   "engine default flows through when no per-emit override");
        assert_eq!(m.get("_trace").and_then(|v| v.as_str()), Some("abc123"),
                   "per-emit-only keys merge in");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_completes_within_a_reasonable_window() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(UserRoster::default())
        .build();

        let start = std::time::Instant::now();
        engine.shutdown().await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(2),
                "shutdown took longer than 2s");
    }

    // ── 0.3.3 — ctx.aggregate restored ──────────────────────────────

    /// Tick fact + event used to drive an aggregator-folded counter.
    /// Uses `:` separator so the legacy aggregator's prefix matcher
    /// (`extract_prefix(event_type)` splits on `:`) finds it.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Tick { seq: u32, occurred_at: DateTime<Utc> }
    impl Fact for Tick {
        const CATEGORY: &'static str = "ticker";
        fn name(&self) -> &str { "tick" }
        fn stream_id(&self) -> Uuid { Uuid::nil() }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    struct TickCounter { count: u32 }
    impl crate::aggregate_v3::Aggregate for TickCounter {
        const NAME: &'static str = "TickCounter";
    }
    impl crate::aggregate_v3::Apply<Tick> for TickCounter {
        fn apply(&mut self, _t: &Tick) { self.count += 1; }
    }

    fn tick_aggregator() -> crate::aggregator::Aggregator {
        crate::aggregator::Aggregator::for_type::<TickCounter, Tick>()
    }

    #[tokio::test]
    async fn projector_ctx_aggregate_curr_includes_current_event() {
        // Projector reads ctx.aggregate during project() and
        // captures `curr.count` for each event. After 3 ticks it
        // should see [1, 2, 3] — runner folds the event into the
        // registry BEFORE invoking project.
        #[derive(Clone)]
        struct Capture { snaps: Arc<parking_lot::Mutex<Vec<u32>>> }
        #[async_trait]
        impl Projector for Capture {
            type Fact = Tick;
            const GROUP_NAME: &'static str = "ticks";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                let s = ctx.aggregate::<TickCounter>().curr;
                self.snaps.lock().push(s.count);
                Ok(())
            }
        }

        let store = store();
        let cap = Capture { snaps: Arc::new(parking_lot::Mutex::new(Vec::new())) };
        let snaps = cap.snaps.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators(vec![tick_aggregator()])
        .with_projector(cap)
        .build();

        for i in 0..3 {
            engine.emit(Tick { seq: i, occurred_at: Utc::now() }).await.unwrap();
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if snaps.lock().len() == 3 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "projector did not catch up within 3s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*snaps.lock(), vec![1, 2, 3],
                   "ctx.aggregate.curr reflects the post-fold state, \
                    including the current event");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reactor_ctx_aggregate_sees_prev_to_curr_transition() {
        // Reactor reads (prev, curr) pair around the fold. After event N,
        // prev.count == N-1 and curr.count == N. Reactor captures both;
        // we assert the deltas are exactly +1 each event.
        #[derive(Clone)]
        struct Capture {
            transitions: Arc<parking_lot::Mutex<Vec<(u32, u32)>>>,
        }
        #[async_trait]
        impl Reactor for Capture {
            type Trigger = Tick;
            const GROUP_NAME: &'static str = "ticker.reactor";
            async fn react(
                &self, _t: &Tick, ctx: Ctx<'_>,
            ) -> Result<crate::reactor_v3::Events> {
                let s = ctx.aggregate::<TickCounter>();
                self.transitions.lock().push((s.prev.count, s.curr.count));
                Ok(crate::reactor_v3::Events::new())
            }
        }

        let store = store();
        let cap = Capture { transitions: Arc::new(parking_lot::Mutex::new(Vec::new())) };
        let transitions = cap.transitions.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators(vec![tick_aggregator()])
        .with_reactor(cap)
        .build();

        for i in 0..3 {
            engine.emit(Tick { seq: i, occurred_at: Utc::now() }).await.unwrap();
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if transitions.lock().len() == 3 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "reactor did not see all 3 ticks within 3s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*transitions.lock(), vec![(0, 1), (1, 2), (2, 3)],
                   "(prev, curr) tracks the fold across each event");

        engine.shutdown().await.unwrap();
    }

    /// Append a Tick directly to the store, return its position.
    async fn append_tick(store: &MemoryStore, seq: u32) -> LogCursor {
        let tick = Tick { seq, occurred_at: Utc::now() };
        let result = EventLogBackend::append(store, NewEvent {
            event_id:        Uuid::new_v4(),
            parent_id:       None,
            correlation_id:  Uuid::new_v4(),
            event_type:      "ticker:tick".into(),
            payload:         serde_json::to_value(&tick).unwrap(),
            created_at:      tick.occurred_at,
            aggregate_type:  None,
            aggregate_id:    None,
            metadata:        Metadata::new(),
            ephemeral:       None,
            persistent:      true,
        }).await.unwrap();
        result.position
    }

    /// Build a fresh aggregator registry that folds Tick→TickCounter (singleton).
    fn fresh_tick_registry() -> Arc<crate::aggregator::AggregatorRegistry> {
        let mut r = crate::aggregator::AggregatorRegistry::new();
        r.register(tick_aggregator());
        Arc::new(r)
    }

    #[tokio::test]
    async fn project_failure_rolls_back_aggregator_fold() {
        // Projector succeeds on event 1, errors on event 2. After
        // the failed step, the aggregator registry should hold count=1
        // — event 2's fold was rolled back via capture/restore. Without
        // rollback, count would be 2.
        struct FailsOnSecond { calls: Arc<AtomicUsize> }
        #[async_trait]
        impl Projector for FailsOnSecond {
            type Fact = Tick;
            const GROUP_NAME: &'static str = "rollback.test";
            async fn project(
                &self, _f: &Tick, _ctx: Ctx<'_>,
            ) -> Result<()> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 2 { Err(anyhow::anyhow!("simulated failure on call {}", n)) } else { Ok(()) }
            }
        }

        let store = Arc::new(MemoryStore::new());
        append_tick(&store, 0).await;
        append_tick(&store, 1).await;

        let aggs = fresh_tick_registry();
        let runner = ProjectionRunner::new(
            FailsOnSecond { calls: Arc::new(AtomicUsize::new(0)) },
            "rollback.test",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        ).with_aggregators(aggs.clone());

        let result = runner.step(10).await;
        assert!(result.is_err(), "step propagates the project error");

        let (_, curr) = aggs.get_singleton_arc::<TickCounter>();
        assert_eq!(curr.count, 1,
                   "event 2's fold rolled back; only event 1's fold remains");
    }

    #[tokio::test]
    async fn hydration_does_not_double_fold_after_zero_cursor_first_step() {
        // Reproduces the OnceCell short-circuit bug: first step at
        // cursor=ZERO must initialize the hydration guard so that a
        // subsequent step (now at cursor>0) doesn't re-replay events
        // that were already folded by step 1.
        //
        // Without the fix, projector sees curr.count = [1, 3, 5]
        // (each step after the first re-folds prior events). With the
        // fix, [1, 2, 3].
        #[derive(Clone)]
        struct Capture { snaps: Arc<parking_lot::Mutex<Vec<u32>>> }
        #[async_trait]
        impl Projector for Capture {
            type Fact = Tick;
            const GROUP_NAME: &'static str = "hydration.bug";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                self.snaps.lock().push(ctx.aggregate::<TickCounter>().curr.count);
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let cap = Capture { snaps: Arc::new(parking_lot::Mutex::new(Vec::new())) };
        let snaps = cap.snaps.clone();

        let runner = ProjectionRunner::new(
            cap,
            "hydration.bug",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        ).with_aggregators(fresh_tick_registry());

        // 3 sequential append→step cycles. First step has cursor=ZERO,
        // subsequent steps have cursor>0 — the bug-prone path.
        for i in 0..3 {
            append_tick(&store, i).await;
            runner.step(10).await.unwrap();
        }

        assert_eq!(*snaps.lock(), vec![1, 2, 3],
                   "each step folds exactly one new event; no replay double-folding");
    }

    #[tokio::test]
    async fn fresh_runner_hydrates_log_when_starting_at_nonzero_cursor() {
        // Process-restart scenario: log has 2 historical events, the
        // checkpoint store says the consumer already processed them,
        // a 3rd event has just landed. A fresh runner with a fresh
        // (empty) registry must hydrate via log replay so its
        // ctx.aggregate.curr reflects the historical folds plus the new one.
        let store = Arc::new(MemoryStore::new());
        append_tick(&store, 0).await;
        let pos2 = append_tick(&store, 1).await;
        // Pre-set checkpoint past the 2 historical events.
        store.set("hydrate.cold", pos2).await.unwrap();
        // The new event the runner picks up after hydration.
        append_tick(&store, 2).await;

        #[derive(Clone)]
        struct Capture { snap: Arc<parking_lot::Mutex<Option<u32>>> }
        #[async_trait]
        impl Projector for Capture {
            type Fact = Tick;
            const GROUP_NAME: &'static str = "hydrate.cold";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                *self.snap.lock() = Some(ctx.aggregate::<TickCounter>().curr.count);
                Ok(())
            }
        }

        let cap = Capture { snap: Arc::new(parking_lot::Mutex::new(None)) };
        let snap = cap.snap.clone();

        let runner = ProjectionRunner::new(
            cap,
            "hydrate.cold",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        ).with_aggregators(fresh_tick_registry());

        runner.step(10).await.unwrap();

        assert_eq!(*snap.lock(), Some(3),
                   "hydration folded the 2 historical events; step folded the new one");
    }

    #[tokio::test]
    #[should_panic(expected = "no aggregators were registered")]
    async fn ctx_aggregate_panics_without_registered_aggregators() {
        // Projector body calls ctx.aggregate but engine has no
        // aggregator registry — must panic with a clear message rather
        // than silently returning default state. Drives a runner
        // directly (skip the engine + supervisor) so the panic
        // propagates to the test task.
        struct Reader;
        #[async_trait]
        impl Projector for Reader {
            type Fact = Tick;
            const GROUP_NAME: &'static str = "reader";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                let _ = ctx.aggregate::<TickCounter>().curr;
                Ok(())
            }
        }

        let store = store();
        // Append one Tick directly to the log.
        let tick = Tick { seq: 0, occurred_at: Utc::now() };
        let payload = serde_json::to_value(&tick).unwrap();
        EventLogBackend::append(store.as_ref(), NewEvent {
            event_id:        Uuid::new_v4(),
            parent_id:       None,
            correlation_id:  Uuid::new_v4(),
            event_type:      "ticker:tick".into(),
            payload,
            created_at:      tick.occurred_at,
            aggregate_type:  None,
            aggregate_id:    None,
            metadata:        Metadata::new(),
            ephemeral:       None,
            persistent:      true,
        }).await.unwrap();

        let runner = ProjectionRunner::new(
            Reader,
            "reader",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );
        // The step() future will panic when project calls
        // ctx.aggregate without an aggregator registry attached.
        let _ = runner.step(10).await;
    }

    // ── 0.3.5 — operational hardening ────────────────────────────────

    #[tokio::test]
    async fn supervisor_recovers_from_consumer_panic() {
        // Projector panics on the 1st call, succeeds on the 2nd.
        // Without panic handling in supervise_one, the spawned tokio
        // task dies on panic, the consumer never advances cursor, and
        // the engine has a silent dead consumer.
        //
        // With the supervisor catching panics, the supervisor logs at
        // ERROR, backs off, then retries — and the projector
        // eventually catches up. This test fails without the catch
        // (deadline elapses with seen.len() == 0).
        #[derive(Clone)]
        struct PanicsThenSucceeds {
            calls: Arc<AtomicUsize>,
            seen:  Arc<parking_lot::Mutex<Vec<Uuid>>>,
        }
        #[async_trait]
        impl Projector for PanicsThenSucceeds {
            type Fact = Tick;
            const GROUP_NAME: &'static str = "panic.recovery";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 { panic!("simulated panic on first call"); }
                self.seen.lock().push(ctx.event_id);
                Ok(())
            }
        }

        let store = store();
        let m = PanicsThenSucceeds {
            calls: Arc::new(AtomicUsize::new(0)),
            seen:  Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let seen = m.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_projector(m)
        .build();

        engine.emit(Tick { seq: 0, occurred_at: Utc::now() }).await.unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if seen.lock().len() == 1 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "supervisor died on panic; consumer never recovered");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn with_aggregators_chainable_accumulation() {
        // Two aggregator definitions registered via two separate
        // with_aggregators calls. Both must fold the same Tick events
        // — the second call accumulates onto the first, doesn't
        // replace it. Mirrors rootsignal's call pattern:
        //   .with_aggregators(pipeline_aggregators::aggregators())
        //   .with_aggregators(curiosity_aggregators::aggregators())

        #[derive(Debug, Default, Clone, Serialize, Deserialize)]
        struct OtherCounter { count: u32 }
        impl crate::aggregate_v3::Aggregate for OtherCounter {
            const NAME: &'static str = "OtherCounter";
        }
        impl crate::aggregate_v3::Apply<Tick> for OtherCounter {
            fn apply(&mut self, _: &Tick) { self.count += 1; }
        }

        #[derive(Clone)]
        struct VerifyBoth {
            a: Arc<parking_lot::Mutex<Vec<u32>>>,
            b: Arc<parking_lot::Mutex<Vec<u32>>>,
        }
        #[async_trait]
        impl Projector for VerifyBoth {
            type Fact = Tick;
            const GROUP_NAME: &'static str = "accum.test";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                self.a.lock().push(ctx.aggregate::<TickCounter>().curr.count);
                self.b.lock().push(ctx.aggregate::<OtherCounter>().curr.count);
                Ok(())
            }
        }

        let agg_a = crate::aggregator::Aggregator::for_type::<TickCounter, Tick>();
        let agg_b = crate::aggregator::Aggregator::for_type::<OtherCounter, Tick>();

        let store = store();
        let v = VerifyBoth {
            a: Arc::new(parking_lot::Mutex::new(Vec::new())),
            b: Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let oa = v.a.clone();
        let ob = v.b.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregators(vec![agg_a])     // first call
        .with_aggregators(vec![agg_b])     // second call — must accumulate
        .with_projector(v)
        .build();

        for i in 0..2 {
            engine.emit(Tick { seq: i, occurred_at: Utc::now() }).await.unwrap();
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if oa.lock().len() == 2 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "projector didn't catch up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(*oa.lock(), vec![1, 2],
                   "TickCounter (1st with_aggregators) folded each event");
        assert_eq!(*ob.lock(), vec![1, 2],
                   "OtherCounter (2nd with_aggregators) folded each event — \
                    accumulation, not replacement");

        engine.shutdown().await.unwrap();
    }

    // ── 0.4 — MultiProjector end-to-end ──────────────────────────

    #[tokio::test]
    async fn engine_drives_multi_projector_filtering_subscription() {
        // End-to-end: register a multi-projector subscribed to
        // category `ticker` only, emit a Tick (matches), emit a
        // different-category fact (filtered before body sees it).
        // Projector's seen-list reflects only the subscribed events.
        use crate::multi_projector::MultiProjector;

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct OtherFact {
            id: Uuid,
            occurred_at: DateTime<Utc>,
        }
        impl Fact for OtherFact {
            const CATEGORY: &'static str = "other";
            fn name(&self) -> &str { "happening" }
            fn stream_id(&self) -> Uuid { self.id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        #[derive(Clone)]
        struct OnlyTickRouter {
            seen: Arc<parking_lot::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl MultiProjector for OnlyTickRouter {
            const GROUP_NAME: &'static str = "tick.only";
            const CATEGORIES: &'static [&'static str] = &["ticker"];
            async fn project(
                &self,
                event: &crate::types::PersistedEvent,
                _ctx: Ctx<'_>,
            ) -> Result<()> {
                self.seen.lock().push(event.event_type.clone());
                Ok(())
            }
        }

        let store = store();
        let router = OnlyTickRouter {
            seen: Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let seen = router.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_multi_projector(router)
        .build();

        engine.emit(Tick { seq: 0, occurred_at: Utc::now() }).await.unwrap();
        engine.emit(OtherFact { id: Uuid::new_v4(), occurred_at: Utc::now() }).await.unwrap();
        engine.emit(Tick { seq: 1, occurred_at: Utc::now() }).await.unwrap();

        // Wait for the cursor to advance past all 3 events. We don't
        // know the ticker count up-front because of timing — but we
        // do know exactly 2 ticker events should be delivered.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if seen.lock().len() == 2 { break; }
            assert!(std::time::Instant::now() < deadline,
                    "projector didn't see expected 2 ticker events");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let s = seen.lock();
        assert_eq!(s.len(), 2, "exactly 2 events delivered (the OtherFact filtered)");
        assert!(s.iter().all(|t| t.starts_with("ticker:")),
                "every delivered event matches the declared subscription");

        engine.shutdown().await.unwrap();
    }
}
