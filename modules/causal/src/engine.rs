//! `Engine` + `EngineBuilder` — the public runtime surface.
//!
//! Wires per-consumer runners into one supervisor: each registered
//! reactor / projector / multi-projector spawns its own task, plus
//! reactor/projector runner supervisors (reactors append outputs directly).
//! The builder casts backend trait objects (`EventLogBackend`,
//! `CheckpointStore`, `ReactorCheckpoint`) and assembles them.

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

use crate::aggregate::{Aggregate, Apply};
use crate::aggregator::{Aggregator, AggregatorRegistry};
use crate::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use crate::multi_projector::{MultiProjector, MultiProjectorRunner};
use crate::contexts::Metadata;
use crate::event_log::EventLogBackend;
use crate::event::Event;
use crate::projector::Projector;
use crate::projection_runner::{ProjectionRunner, StepOutcome};
use crate::reactor_runner::ReactorRunner;
use crate::reactor::Reactor;
use crate::types::{LogCursor, EventData, StreamRevision};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const BACKOFF_ON_ERROR: Duration = Duration::from_millis(250);
const SUPERVISOR_BATCH: usize = 256;

// ─────────────────────────────────────────────────────────────────────
// Supervisable — trait-object adapter so we can box heterogeneous
// runners (ProjectionRunner<M>, ViewRunner<V>, ReactorRunner<R>) into
// a single Vec for the supervisor pool.
// ─────────────────────────────────────────────────────────────────────

#[async_trait]
trait Supervisable: Send + Sync {
    async fn step(&self, batch: usize) -> Result<StepOutcome>;
    fn consumer_id(&self) -> &str;
    /// The settle probe: has this consumer fully finished workflow `wf`
    /// up to high-water `hw`? Serial consumers answer with their durable
    /// cursor; the partitioned reactor runner answers with its
    /// ingestion cursor + per-workflow pending work, so one wedged
    /// partition can't hold an unrelated workflow's settle hostage.
    async fn drained(&self, wf: Uuid, hw: LogCursor) -> Result<bool>;
    /// Stop taking new work (in-flight bodies finish). Default no-op
    /// for serial consumers, whose supervisor exit is sufficient.
    fn halt(&self) {}
}

#[async_trait]
impl<P: Projector + 'static> Supervisable for ProjectionRunner<P>
where
    P::Event: DeserializeOwned,
{
    async fn step(&self, batch: usize) -> Result<StepOutcome> {
        ProjectionRunner::step(self, batch).await
    }
    fn consumer_id(&self) -> &str { ProjectionRunner::consumer_id(self) }
    async fn drained(&self, _wf: Uuid, hw: LogCursor) -> Result<bool> {
        Ok(ProjectionRunner::cursor(self).await?.is_some_and(|c| c >= hw))
    }
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
    async fn drained(&self, wf: Uuid, hw: LogCursor) -> Result<bool> {
        ReactorRunner::drained(self, wf, hw).await
    }
    fn halt(&self) { ReactorRunner::halt(self) }
}

#[async_trait]
impl<P: MultiProjector + 'static> Supervisable for MultiProjectorRunner<P> {
    async fn step(&self, batch: usize) -> Result<StepOutcome> {
        MultiProjectorRunner::step(self, batch).await
    }
    fn consumer_id(&self) -> &str { MultiProjectorRunner::consumer_id(self) }
    async fn drained(&self, _wf: Uuid, hw: LogCursor) -> Result<bool> {
        Ok(MultiProjectorRunner::cursor(self).await?.is_some_and(|c| c >= hw))
    }
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
    P::Event: DeserializeOwned,
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
/// `workflow_id` is the chain id stamped on every fact in the
/// batch. Clients use it to poll workflow-status projections;
/// tests use it to scope reads (`engine.state_of::<A>(corr_id).await.unwrap()`)
/// and waits (`engine.settled()`) to a single chain. Auto-generated
/// per emit unless the caller set it via `EmitBuilder::workflow_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmitResult {
    pub position:       LogCursor,
    pub workflow_id: Uuid,
}

/// In-process per-workflow high-water mark for scoped [`Engine::settle`].
///
/// Tracks, per `workflow_id`, the highest `$all` position of any event in
/// that run's causal chain — seeded floor-wise by the emit position and bumped
/// by each reactor runner as it appends an output (outputs inherit the
/// trigger's `workflow_id`, so the whole chain shares one key). `settle`
/// reads it to wait only for *its* run to drain, not for global log quiescence.
///
/// Bounded: an entry is created lazily (first output for a run) and removed when
/// `settle` returns; a hard `CAP` evicts an arbitrary entry under fire-and-forget
/// load (emit without `.settled()`), trading a possible early `settle` return
/// under pathological concurrency for bounded memory. This is in-process state —
/// correct when a run's reactors execute in the same engine that called
/// `settle`. A multi-engine deployment would need a backend-queried high-water.
pub(crate) struct SettleTracker {
    hw:  std::collections::HashMap<Uuid, LogCursor>,
}

/// Cap on tracked in-flight workflows. Generous — never approached when
/// `.settled()` is used (entries are removed on return); only fire-and-forget
/// emits accumulate, and this bounds them.
const SETTLE_TRACKER_CAP: usize = 65_536;

impl SettleTracker {
    fn new() -> Self {
        Self { hw: std::collections::HashMap::new() }
    }

    /// Record a chain event's position for `wf`, keeping the max.
    pub(crate) fn bump(&mut self, wf: Uuid, pos: LogCursor) {
        if let Some(cur) = self.hw.get_mut(&wf) {
            if pos > *cur {
                *cur = pos;
            }
            return;
        }
        if self.hw.len() >= SETTLE_TRACKER_CAP {
            // Bound memory under fire-and-forget load. Evicting a currently
            // settling run's entry makes that settle fall back to its emit-
            // position floor (possible early return) — acceptable only under
            // >CAP un-settled concurrent runs.
            if let Some(&victim) = self.hw.keys().next() {
                self.hw.remove(&victim);
            }
        }
        self.hw.insert(wf, pos);
    }

    fn get(&self, wf: &Uuid) -> Option<LogCursor> {
        self.hw.get(wf).copied()
    }

    fn forget(&mut self, wf: &Uuid) {
        self.hw.remove(wf);
    }
}

/// Shared handle to the per-workflow high-water tracker, threaded from the
/// engine into each reactor runner.
pub(crate) type WorkflowHighWater = Arc<std::sync::Mutex<SettleTracker>>;

/// Metadata about a reactor that has exhausted its retry budget,
/// passed to the [`EngineBuilder::on_terminal_failure`] mapper. The mapper
/// decides whether to synthesize a terminal-failure Event and append
/// it to its stream so downstream consumers can react.
///
/// Production use case: scout's `PipelineEvent::HandlerFailed` is
/// emitted on terminal reactor failure so `PipelineState` can fold
/// it and unblock downstream gates.
#[derive(Debug, Clone)]
pub struct TerminalFailure {
    /// `Reactor::NAME` of the failing reactor.
    pub consumer:        String,
    /// `event_id` of the trigger that caused the failure.
    pub trigger_id:   Uuid,
    /// `event_type` of the trigger (the fact kind, verbatim).
    pub trigger_event_type: String,
    /// The trigger's subject placement category — with `subject_id`,
    /// the subject history the trigger belongs to. BLOCKING-2's
    /// contract: a terminal failure mid-workflow means the workflow's
    /// completion event never arrives, so downstream completion logic
    /// MUST be able to fold the failure as completion-with-error — the
    /// mapper has the subject identity to stamp its fact with.
    pub subject:           String,
    /// The trigger's `subject_id`.
    pub subject_id:        Uuid,
    /// How the failing error was classified (BLOCKING-2):
    /// `transient_exhausted` is mass-replayable as a class after a real
    /// outage; `unclassified` is visible instead of masquerading as a
    /// declared `domain`.
    pub class:             crate::failure::FailureClass,
    /// Last error message from the reactor's `react()`.
    pub error:             String,
    /// Number of attempts that ran before declaring terminal failure.
    pub attempts:          u32,
    /// `workflow_id` of the failing trigger — its run / causal chain.
    /// The terminal-failure-synthesized event already inherits this; exposing it lets the
    /// mapper key its terminal-failure event per-run (e.g. stream-by-`run_id`)
    /// so a dead-letter can still unblock that run's downstream gates.
    pub workflow_id:    Uuid,
}

/// Type-erased view of a [`Event`] for the emit builder.
///
/// `EmitInput` stores facts behind this trait so [`EmitBuilder`] is
/// non-generic — one builder type handles any Event, single or
/// batched. The blanket impl below covers every `Event` automatically;
/// downstream code never names this trait.
pub(crate) trait ErasedFact: Send + Sync {
    /// The fact's kind — `Event::NAME`, the exact wire `event_type`.
    fn name(&self) -> &'static str;
    /// Subject history this fact joins (`Event::SUBJECT`; defaults to
    /// the kind).
    fn subject(&self) -> &'static str;
    fn subject_id(&self) -> Uuid;
    fn occurred_at(&self) -> Option<DateTime<Utc>>;
    /// `Some` = workflow root (see `Event::declared_workflow_id`).
    fn declared_workflow_id(&self) -> Option<Uuid>;
    fn to_value(&self) -> Result<serde_json::Value>;
}

impl<F: Event> ErasedFact for F {
    fn name(&self) -> &'static str { <F as Event>::NAME }
    fn subject(&self) -> &'static str { <F as Event>::SUBJECT }
    fn subject_id(&self) -> Uuid { Event::subject_id(self) }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Event::occurred_at(self) }
    fn declared_workflow_id(&self) -> Option<Uuid> { Event::declared_workflow_id(self) }
    fn to_value(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).map_err(Into::into)
    }
}

/// What `Engine::emit` accepts: a single Event, or a batch of Facts
/// of the same type. Both produced automatically via `Into` impls —
/// callers write `engine.emit(fact)` or `engine.emit(vec![f1, f2])`.
pub struct EmitInput {
    facts: Vec<Box<dyn ErasedFact>>,
}

impl<F: Event> From<F> for EmitInput {
    fn from(f: F) -> Self {
        Self { facts: vec![Box::new(f) as Box<dyn ErasedFact>] }
    }
}

impl<F: Event> From<Vec<F>> for EmitInput {
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
/// any Event, single or batched.
pub struct EmitBuilder<'a> {
    engine:         &'a Engine,
    input:          EmitInput,
    workflow_id: Option<Uuid>,
    causation_id:      Option<Uuid>,
    metadata:       Metadata,
}

impl<'a> EmitBuilder<'a> {
    /// Stamp `workflow_id` on every fact in the batch. Defaults to
    /// a fresh UUID per emit; command handlers should propagate the
    /// trigger's `workflow_id` here so causal-chain tracing works
    /// across the system.
    pub fn workflow_id(mut self, id: Uuid) -> Self {
        self.workflow_id = Some(id);
        self
    }

    /// Stamp `causation_id` on every fact in the batch. Defaults to
    /// `None` (root event). Command handlers should pass the trigger's
    /// `event_id` here.
    pub fn causation_id(mut self, id: Uuid) -> Self {
        self.causation_id = Some(id);
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
    /// return a workflow_id to the client, use the bare
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

/// Cross-cutting wiring handed to every consumer factory at `build()`
/// time. Factories receive the builder's FINAL configuration — never a
/// snapshot captured at registration — so `.with_reactor(r)
/// .on_terminal_failure(..)` and `.on_terminal_failure(..)
/// .with_reactor(r)` mean the same thing. (The eager-capture version
/// silently dropped any mapper/observer/effect-store/clock configured
/// after a consumer was registered — an ordering-dependent lying
/// default.)
pub(crate) struct ConsumerWiring {
    /// Fresh per-consumer aggregator registry (fold-function table for
    /// reactors; scan-folded state for serial projectors).
    pub aggs: Option<Arc<AggregatorRegistry>>,
    /// The shared engine-level registry.
    pub engine_aggs: Option<Arc<AggregatorRegistry>>,
    pub occ: Arc<std::collections::HashSet<String>>,
    pub failure_mapper: Option<TerminalFailureMapper>,
    pub max_attempts: u32,
    pub observer: Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
    pub effect_store: Option<Arc<dyn crate::effect_store::EffectStore>>,
    pub workflow_hw: WorkflowHighWater,
    pub snapshot_store: Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
    pub snapshot_every: u64,
    pub clock: Arc<dyn crate::clock::Clock>,
}

/// Constructs an `Arc<dyn Supervisable>` runner at `build()` time,
/// once the builder's full configuration has accumulated.
type RunnerFactory = Box<dyn FnOnce(ConsumerWiring) -> Arc<dyn Supervisable> + Send>;

/// Type-erased terminal-failure mapper plumbed from
/// `EngineBuilder::on_terminal_failure` into each `ReactorRunner`.
pub(crate) type TerminalFailureMapper = Arc<
    dyn Fn(TerminalFailure) -> Option<Box<dyn ErasedFact>> + Send + Sync,
>;

/// Framework default for `max_attempts` when `on_terminal_failure` is configured.
/// Reactors retry up to this many times before the mapper fires.
pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Default snapshot cadence: save an aggregate snapshot every N folded events.
pub(crate) const DEFAULT_SNAPSHOT_EVERY: u64 = 100;

pub struct EngineBuilder {
    log:                   Arc<dyn EventLogBackend>,
    checkpoint:            Arc<dyn CheckpointStore>,
    reactor_checkpoint:    Arc<dyn ReactorCheckpoint>,
    consumers:             Vec<RunnerFactory>,
    aggregators:           Vec<Aggregator>,
    consumer_names:           std::collections::HashSet<String>,
    /// Categories registered via [`with_aggregate`](Self::with_aggregate)
    /// as `StreamPolicy::OccRequired`. `Engine::emit` rejects facts in
    /// these categories — they must go through the OCC command path
    /// [`Engine::append`].
    occ_categories:        std::collections::HashSet<String>,
    default_metadata:      Metadata,
    failure_mapper:            Option<TerminalFailureMapper>,
    max_attempts:          u32,
    observer:              Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
    /// Reaction-result cache (Phase 4). Plumbed into every `ReactorRunner`
    /// registered *after* this is set (same ordering rule as `observer`),
    /// surfaced to reactor bodies via `ctx.effect_store()`.
    effect_store:        Option<Arc<dyn crate::effect_store::EffectStore>>,
    /// Per-workflow high-water tracker for scoped `settle`. Created eagerly
    /// (so registration order doesn't matter), shared with every reactor runner
    /// and the built engine.
    workflow_hw:               WorkflowHighWater,
    /// Durable aggregate snapshot store. When set, folded aggregate state
    /// survives restart via read-through restore (`Engine::state_of`,
    /// consumer restore-before-fold) and is periodically snapshotted. When
    /// `None`, behavior is unchanged (in-memory fold only).
    snapshot_store:        Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
    /// Save a snapshot every N folded events per aggregate. `0` disables saving
    /// (restore still works via full replay).
    snapshot_every:        u64,
    /// `(consumer, start_position)` per registered reactor — consumed by
    /// `build()` to seed absent reactor cursors BEFORE consumers spawn.
    /// Projectors are deliberately absent: read models start from ZERO
    /// (they want full history); side effects must not replay it.
    reactor_seeds:         Vec<(&'static str, crate::projection::StartPosition)>,
    /// Calendar clock for framework-stamped timestamps (`created_at`
    /// fallbacks, terminal-failure record times). Injected so tests pin
    /// it; defaults to the system clock. Liveness decisions never use
    /// it (Primitive 6's two-clock rule).
    clock:                 Arc<dyn crate::clock::Clock>,
}

impl EngineBuilder {
    /// `checkpoint` stores projector/reactor cursors; `reactor_checkpoint`
    /// adds the reactor retry-attempt counters. They typically point to
    /// the same backend instance (e.g., one `Arc<MemoryStore>` cast to
    /// both traits). An engine hosting no reactors only ever touches
    /// `checkpoint`.
    pub fn new(
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
        reactor_checkpoint: Arc<dyn ReactorCheckpoint>,
    ) -> Self {
        Self {
            log,
            checkpoint,
            reactor_checkpoint,
            consumers: Vec::new(),
            aggregators: Vec::new(),
            consumer_names: std::collections::HashSet::new(),
            occ_categories: std::collections::HashSet::new(),
            default_metadata: Metadata::new(),
            failure_mapper: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            observer: None,
            effect_store: None,
            workflow_hw: Arc::new(std::sync::Mutex::new(SettleTracker::new())),
            snapshot_store: None,
            snapshot_every: DEFAULT_SNAPSHOT_EVERY,
            reactor_seeds: Vec::new(),
            clock: Arc::new(crate::clock::SystemClock),
        }
    }

    /// Inject the calendar clock. Every timestamp the framework stamps
    /// (`created_at` when a fact declares no `occurred_at`,
    /// terminal-failure record times, observer attempt times) comes
    /// from it — `FixedClock` in tests makes appended envelopes
    /// reproducible. Replay is byte-stable only when all three hold:
    /// injected clock, effects memoized (`ctx.effect`), canonical
    /// payload serialization.
    pub fn with_clock(mut self, clock: Arc<dyn crate::clock::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Register a [`EffectStore`](crate::effect_store::EffectStore)
    /// surfaced to reactor bodies via `ctx.effect_store()`. Lets a
    /// side-effecting reactor memoize its external call under its
    /// [`EffectKey`](crate::effect_store::EffectKey) so retry /
    /// redelivery runs the call effectively once.
    ///
    /// Ordering: like [`with_observer`](Self::with_observer), this is
    /// plumbed into reactors registered *after* this call. Set it before
    /// `with_reactor(...)`.
    pub fn with_effect_store(
        mut self,
        cache: Arc<dyn crate::effect_store::EffectStore>,
    ) -> Self {
        self.effect_store = Some(cache);
        self
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
    ///         store.clone() as Arc<dyn ReactorCheckpoint>,
    ///     )
    ///     .with_observer(store.clone())
    ///     .with_reactor(MyReactor)
    ///     .build().await.unwrap();
    /// ```
    ///
    /// Takes `Arc<dyn ReactorObserver>` so callers holding a trait
    /// object (e.g. one chosen at runtime) can pass it directly; a
    /// concrete `Arc<MyObserver>` coerces at the call site.
    pub fn with_observer(
        mut self,
        observer: Arc<dyn crate::reactor_observer::ReactorObserver>,
    ) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Register a terminal-failure mapper, replacing the built-in
    /// terminal fact. When a trigger exhausts its retry policy (per
    /// the BLOCKING-2 class taxonomy — see [`crate::failure`]), the
    /// runner invokes the mapper with the failure details. If the
    /// mapper returns `Some(fact)`, the fact is appended to its own
    /// stream (so downstream consumers react to it); either way the
    /// trigger parks and its partition moves on.
    ///
    /// **The terminal path is mandatory, not optional.** Without a
    /// mapper, the runner appends the built-in fact
    /// `causal:reaction_failed { consumer, trigger_id,
    /// trigger_event_type, class, error, attempts }` to the TRIGGER's
    /// own subject history — so a completion fold over that subject
    /// can see the failure. There is no retry-forever mode; the
    /// transient class's ceilinged backoff is the sanctioned "wait out
    /// the outage" behavior.
    ///
    /// **Contract for whoever consumes the terminal fact** (mapper
    /// output or built-in): a terminal failure mid-workflow means the
    /// workflow's completion event never arrives. Downstream completion
    /// logic MUST fold terminal-failure facts as completion-with-error,
    /// or workflows leak (a run wedged "running" busy-gates its source
    /// forever). `TerminalFailure` carries the trigger's `subject` /
    /// `subject_id` so the mapper can stamp its fact accordingly.
    ///
    /// Use case in scout: synthesize `PipelineEvent::HandlerFailed`
    /// from `TerminalFailure`; `PipelineState` folds it and unblocks
    /// downstream gates based on `info.consumer`.
    pub fn on_terminal_failure<F, Out>(mut self, mapper: F) -> Self
    where
        F: Fn(TerminalFailure) -> Option<Out> + Send + Sync + 'static,
        Out: Event,
    {
        self.failure_mapper = Some(Arc::new(move |info| {
            mapper(info).map(|f| Box::new(f) as Box<dyn ErasedFact>)
        }));
        self
    }

    /// Override the framework's default retry budget for
    /// domain-class / unclassified reactor errors. Default is
    /// [`DEFAULT_MAX_ATTEMPTS`] (3). Transient-class errors are
    /// governed by the liveness-time ceiling, not this budget; poison
    /// parks immediately (see [`crate::failure`]).
    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    /// Wire a durable [`SnapshotStore`](crate::snapshot_store::SnapshotStore)
    /// so folded aggregate state survives restart. With it set, an aggregate
    /// that declares an
    /// [`Aggregate::SUBJECT`](crate::aggregate::Aggregate::SUBJECT)
    /// is restored read-through (snapshot + stream-tail replay) by
    /// [`Engine::state_of`] and by consumer runners before they fold, and
    /// is snapshotted every [`with_snapshot_every`](Self::with_snapshot_every)
    /// events. Without it, behavior is unchanged. Idempotent restore — safe on
    /// any/all nodes.
    pub fn with_snapshot_store(
        mut self,
        store: Arc<dyn crate::snapshot_store::SnapshotStore>,
    ) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// Save an aggregate snapshot every `n` folded events (default
    /// [`DEFAULT_SNAPSHOT_EVERY`]). `0` disables saving — restore still works,
    /// replaying the full stream each time. No effect without
    /// [`with_snapshot_store`](Self::with_snapshot_store).
    pub fn with_snapshot_every(mut self, n: u64) -> Self {
        self.snapshot_every = n;
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

    /// Reserve a `NAME` for a consumer. Panics if another
    /// consumer in this builder already claimed it — two consumers
    /// sharing a cursor key would silently corrupt each other.
    fn claim_name(&mut self, consumer: &'static str) {
        assert!(
            self.consumer_names.insert(consumer.into()),
            "duplicate NAME `{}` registered on EngineBuilder — \
             two consumers MUST NOT share a cursor key",
            consumer,
        );
    }

    /// Register [`crate::Aggregator`] definitions so consumers can read
    /// folded aggregate state via `ctx.state_of::<A>(id)`. Aggregator
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
            // state. Same protective pattern as `claim_name`.
            //
            // Multiple aggregators with the same NAME folding
            // DIFFERENT Event types into one Aggregate (the multi-Event
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
                     same Event CATEGORY `{}` — two aggregators MUST NOT \
                     share a registry key. Multi-Event aggregates folding \
                     different Event streams are fine (Apply<F1> + Apply<F2> \
                     with the same A::NAME); same Event registered twice \
                     is not.",
                    agg.aggregate_type, existing.event_prefix,
                );
            }
            // INVARIANT aggregates install the OCC fence as part of
            // ordinary registration: their fact kinds are rejected by
            // `emit` and by reactor outputs (C4); `Engine::append` is
            // the only write door. The fence is a property of the
            // state's declaration, not a separate builder call.
            if agg.invariant {
                self.occ_categories.insert(agg.event_prefix.clone());
            }
            self.aggregators.push(agg);
        }
        self
    }

    pub fn with_projector<P: Projector + 'static>(mut self, p: P) -> Self
    where
        P::Event: DeserializeOwned,
    {
        self.claim_name(P::NAME);
        let log = self.log.clone();
        let checkpoint = self.checkpoint.clone();
        self.consumers.push(Box::new(move |w: ConsumerWiring| {
            let mut runner = ProjectionRunner::new(p, P::NAME, log, checkpoint);
            if let Some(aggs) = w.aggs { runner = runner.with_aggregators(aggs); }
            if let Some(obs) = w.observer { runner = runner.with_observer(obs); }
            Arc::new(runner) as Arc<dyn Supervisable>
        }));
        self
    }

    pub fn with_reactor<R: Reactor + 'static>(mut self, r: R) -> Self
    where
        R::Trigger: DeserializeOwned,
    {
        self.claim_name(R::NAME);
        self.reactor_seeds.push((
            R::NAME,
            crate::projection::StartPosition::ResumeOrLatest,
        ));
        let log = self.log.clone();
        let reactor_checkpoint = self.reactor_checkpoint.clone();
        self.consumers.push(Box::new(move |w: ConsumerWiring| {
            let mut runner = ReactorRunner::new(r, R::NAME, log, reactor_checkpoint)
                .with_max_attempts(w.max_attempts)
                .with_clock(w.clock);
            if let Some(aggs) = w.aggs { runner = runner.with_aggregators(aggs); }
            if let Some(mapper) = w.failure_mapper {
                runner = runner.with_terminal_failure(mapper);
            }
            if let Some(obs) = w.observer { runner = runner.with_observer(obs); }
            if let Some(rc) = w.effect_store { runner = runner.with_effect_store(rc); }
            runner = runner.with_engine_aggregators(w.engine_aggs);
            runner = runner.with_settle_tracker(w.workflow_hw);
            runner = runner.with_snapshot_persistence(w.snapshot_store, w.snapshot_every);
            runner = runner.with_occ_categories(w.occ);
            Arc::new(runner) as Arc<dyn Supervisable>
        }));
        self
    }

    /// Register a [`MultiProjector`] — cross-domain projection
    /// consumer with declared subscription. The runner filters events
    /// to those whose `event_type` matches any category in
    /// `P::KINDS` (exact event-kind equality) before invoking the
    /// body. Body receives raw `&RecordedEvent` for cross-domain
    /// payload routing.
    ///
    /// Use when:
    /// - Body needs raw `&RecordedEvent` (heterogeneous payload routing
    ///   that no single typed enum captures), AND
    /// - Subscription is a known-bounded set of categories.
    ///
    /// For single-Event consumers, use [`Self::with_projector`] — it
    /// deserializes for you.
    pub fn with_multi_projector<P: MultiProjector + 'static>(mut self, p: P) -> Self {
        self.claim_name(P::NAME);
        let log = self.log.clone();
        let checkpoint = self.checkpoint.clone();
        self.consumers.push(Box::new(move |w: ConsumerWiring| {
            let mut runner = MultiProjectorRunner::new(p, P::NAME, log, checkpoint);
            if let Some(aggs) = w.aggs { runner = runner.with_aggregators(aggs); }
            if let Some(obs) = w.observer { runner = runner.with_observer(obs); }
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

    /// Override the start position for one reactor at registration
    /// time. The default (plain [`with_reactor`](Self::with_reactor)) is
    /// [`StartPosition::ResumeOrLatest`]: resume a persisted cursor, or
    /// seed a fresh one at `latest_position()` — a newly deployed
    /// reactor must NOT re-fire side effects for the existing log.
    /// `StartPosition::Zero` is the explicit escape hatch for "I *want*
    /// this reactor to process history" (see its replay-hazard docs).
    pub fn with_reactor_start<R: Reactor + 'static>(
        self,
        r: R,
        start: crate::projection::StartPosition,
    ) -> Self
    where
        R::Trigger: DeserializeOwned,
    {
        let mut b = self.with_reactor(r);
        if let Some(seed) = b.reactor_seeds.last_mut() {
            seed.1 = start;
        }
        b
    }

    /// Build the engine: seed absent reactor cursors, then spawn every
    /// consumer's supervisor task.
    ///
    /// Async + fallible since the 2026-06-10 remediation (B2): seeding
    /// reads `latest_position()` and writes cursors, and it MUST happen
    /// before `build` returns — seeding lazily in the runner's first
    /// step would race events emitted right after `build()` (the
    /// canonical `build(); emit().settled()` pattern would
    /// nondeterministically skip its own trigger).
    ///
    /// Projector cursors are never seeded: read models start from
    /// `LogCursor::ZERO` and want full history. The defaults differ on
    /// purpose — side effects must not replay; read models must.
    pub async fn build(self) -> Result<Engine> {
        // Reject categories that can't participate in the
        // `{category}:{name}` format before anything runs — a colon in a
        // category silently desyncs reactor matching from aggregate
        // folding (the aggregate never folds, with no runtime error).
        for agg in &self.aggregators {
            crate::event_type::validate_name("aggregator event", &agg.event_prefix)?;
            if !agg.subject.is_empty() {
                crate::event_type::validate_name("aggregate stream", &agg.subject)?;
            }
        }

        // Seed reactor cursors per their StartPosition, before any
        // consumer can take a step.
        for (group, start) in &self.reactor_seeds {
            use crate::projection::StartPosition::*;
            let existing = self.reactor_checkpoint.get(group).await?;
            let seed = match (start, existing) {
                (ResumeOrLatest, Some(_)) => None,
                (ResumeOrLatest, None)    => Some(self.log.latest_position().await?),
                // Latest deliberately ignores a persisted cursor — the
                // backlog is skipped on every (re)build. Ops escape
                // hatch; see StartPosition::Latest docs.
                (Latest, _)               => Some(self.log.latest_position().await?),
                (Zero, _)                 => Some(LogCursor::ZERO),
                (Specific(c), _)          => Some(*c),
            };
            if let Some(pos) = seed {
                self.reactor_checkpoint.set(group, pos).await?;
            }
        }

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
        let occ_shared = Arc::new(self.occ_categories.clone());
        // Wiring is assembled HERE, from the builder's final state —
        // factories never see a registration-time snapshot, so the
        // order of with_reactor vs on_terminal_failure / with_observer
        // / with_effect_store / with_clock cannot change meaning.
        let consumers: Vec<Arc<dyn Supervisable>> = self.consumers
            .into_iter()
            .map(|f| {
                f(ConsumerWiring {
                    aggs: make_registry(),
                    engine_aggs: engine_aggregators.clone(),
                    occ: occ_shared.clone(),
                    failure_mapper: self.failure_mapper.clone(),
                    max_attempts: self.max_attempts,
                    observer: self.observer.clone(),
                    effect_store: self.effect_store.clone(),
                    workflow_hw: self.workflow_hw.clone(),
                    snapshot_store: self.snapshot_store.clone(),
                    snapshot_every: self.snapshot_every,
                    clock: self.clock.clone(),
                })
            })
            .collect();
        Ok(Engine::start(
            self.log,
            self.checkpoint,
            consumers,
            self.default_metadata,
            engine_aggregators,
            self.observer,
            self.occ_categories,
            self.workflow_hw,
            self.snapshot_store,
            self.snapshot_every,
            self.clock,
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Engine
// ─────────────────────────────────────────────────────────────────────

pub struct Engine {
    log:                   Arc<dyn EventLogBackend>,
    checkpoint:            Arc<dyn CheckpointStore>,
    shutdown_tx:           broadcast::Sender<()>,
    handles:               Vec<JoinHandle<()>>,
    /// The supervised consumers, retained for the `settle` drained
    /// probes and for `halt` on shutdown (each also lives in its
    /// supervisor task via an Arc clone).
    consumers:             Vec<Arc<dyn Supervisable>>,
    default_metadata:      Metadata,
    /// Engine-level aggregator registry for out-of-band read access
    /// via `engine.state_of::<A>(subject_id).await.unwrap()`. Folded on every
    /// successful `emit()`. Each consumer holds its OWN registry
    /// clone for in-body `ctx.state_of` reads — the engine-level
    /// registry exists for the test ergonomic of reading aggregate
    /// state without going through a consumer.
    aggregators:           Option<Arc<AggregatorRegistry>>,
    /// Telemetry hook for inspector / external observability sinks.
    observer:              Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
    /// Categories marked `StreamPolicy::OccRequired` via
    /// `EngineBuilder::with_aggregate`. `emit` rejects facts in these
    /// categories; they must use the OCC command path `Engine::append`.
    occ_categories:        std::collections::HashSet<String>,
    /// Per-workflow high-water tracker (shared with reactor runners) that
    /// scopes [`Engine::settle`] to a single run's causal chain.
    workflow_hw:               WorkflowHighWater,
    /// Durable aggregate snapshot store (shared with runners). `None` = no
    /// durable restore (in-memory fold only).
    snapshot_store:        Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
    /// Snapshot cadence for the engine-level save path.
    snapshot_every:        u64,
    /// Calendar clock for emit-path stamping (see `EngineBuilder::with_clock`).
    clock:                 Arc<dyn crate::clock::Clock>,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    fn start(
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
        consumers: Vec<Arc<dyn Supervisable>>,
        default_metadata: Metadata,
        aggregators: Option<Arc<AggregatorRegistry>>,
        observer: Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
        occ_categories: std::collections::HashSet<String>,
        workflow_hw: WorkflowHighWater,
        snapshot_store: Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
        snapshot_every: u64,
        clock: Arc<dyn crate::clock::Clock>,
    ) -> Self {
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let mut handles = Vec::with_capacity(consumers.len());

        // Each consumer (reactor / projector runner) is supervised; it
        // reads the log from its cursor and, for reactors, appends outputs
        // directly. No relay — reactor outputs go straight to the log.
        for consumer in &consumers {
            let consumer = consumer.clone();
            let mut rx = shutdown_tx.subscribe();
            let task = tokio::spawn(async move {
                supervise_one(consumer, &mut rx).await;
            });
            handles.push(task);
        }

        Self {
            log, checkpoint, shutdown_tx, handles, consumers,
            default_metadata,
            aggregators, observer,
            occ_categories,
            workflow_hw,
            snapshot_store,
            snapshot_every,
            clock,
        }
    }

    /// Emit one or more Facts to the log.
    ///
    /// Returns an [`EmitBuilder`] — chain `.metadata()`,
    /// `.workflow_id()`, `.causation_id()` and finally `.await` to run
    /// the write. `.await` returns once facts are durably in the log;
    /// the reactor chain runs asynchronously after that. Use
    /// `.settled().await` to also wait for the chain to drain.
    ///
    /// ```ignore
    /// // simplest — durable append, returns immediately
    /// engine.emit(fact).await?;
    /// // command-handler envelope — propagate trigger workflow
    /// engine.emit(out)
    ///     .workflow_id(trigger_corr)
    ///     .causation_id(trigger_event_id)
    ///     .await?;
    /// // batch
    /// engine.emit(vec![f1, f2]).await?;
    /// // wait for the whole causal chain (tests, sync handlers)
    /// engine.emit(fact).settled().await?;
    /// ```
    ///
    /// # Atomicity & retries
    ///
    /// Consecutive facts targeting the **same stream** land as one
    /// atomic batch — all or nothing, so a failed emit leaves nothing
    /// behind and retrying it cannot duplicate a torn prefix. A batch
    /// spanning **multiple streams** is atomic per same-stream run, in
    /// input order; a failure between runs leaves earlier runs durable
    /// (the backend primitive is per-stream — there is no cross-stream
    /// transaction). Each `emit` call mints fresh `event_id`s, so
    /// retrying an emit that *succeeded* writes the facts again —
    /// retry only on `Err`.
    pub fn emit<I: Into<EmitInput>>(&self, input: I) -> EmitBuilder<'_> {
        EmitBuilder {
            engine: self,
            input: input.into(),
            workflow_id: None,
            causation_id: None,
            metadata: Metadata::new(),
        }
    }

    /// Hydrate an aggregate by folding its full stream from the log.
    /// Returns the aggregate state and the current stream version
    /// (informational — backend-internal cursor, not used for user-
    /// facing CAS).
    ///
    /// Stream identity comes from `F::NAME` + `id`; the same
    /// convention `Engine::emit` uses on write. Caller picks both
    /// type params so the same `Aggregate` impl can fold different
    /// Event streams (e.g. `load::<PipelineState, ScrapeEvent>`).
    pub async fn load<A, F>(
        &self,
        id: Uuid,
    ) -> Result<(A, StreamRevision)>
    where
        A: Aggregate + Apply<F>,
        F: Event + DeserializeOwned,
    {
        // Read the PHYSICAL stream (SUBJECT), which `emit` and
        // durable restore also use. It may be co-located — holding event
        // types other than `F` — so fold only `F`-typed events while still
        // tracking the stream head revision across all of them.
        let events = self.log.read_stream(F::SUBJECT, id, None).await?;
        let mut agg = A::default();
        let mut revision = StreamRevision::ZERO;
        for event in events {
            revision = event.revision;
            if event.event_type.as_str() != F::NAME {
                continue; // foreign co-located type — not ours to fold
            }
            let fact: F = serde_json::from_value(event.payload)?;
            agg.apply(&fact);
        }
        Ok((agg, revision))
    }

    /// OCC-protected command path — the decider pattern, and the
    /// counterpart to [`with_aggregate`](EngineBuilder::with_aggregate).
    ///
    /// Folds aggregate `A` from stream `{F::NAME}-{id}`, hands the
    /// state to `decide`, then appends the resulting facts with an
    /// expected-revision check. On a concurrent write the backend
    /// returns a [`ConflictError`](crate::event_log::ConflictError);
    /// `append` reloads, re-decides, and retries (bounded).
    ///
    /// `decide` MUST be pure — it may run several times across retries.
    /// An empty `Vec` result is a no-op (no append). `emit` rejects
    /// OCC-required categories; this is their write path.
    ///
    /// A multi-fact decision is appended as **one atomic batch** at the
    /// expected revision (KurrentDB commits the events as a unit) — the
    /// whole decision lands or none of it does, so a crash can't tear it.
    ///
    /// The whole decision is written to the single aggregate stream
    /// `{F::NAME}-{id}` — this is the OCC consistency boundary, so every
    /// emitted fact must belong to aggregate `id`. Each fact's own
    /// [`Event::subject_id`] is therefore expected to equal `id` (debug-asserted)
    /// and is not used for routing here; to affect a *different* aggregate, run
    /// a separate `append` against it.
    pub async fn append<A, F, D>(&self, id: Uuid, decide: D) -> Result<EmitResult>
    where
        A: Aggregate + Apply<F>,
        F: Event + DeserializeOwned + serde::Serialize,
        D: Fn(&A) -> Result<Vec<F>>,
    {
        use crate::types::StreamState;
        // OCC retry budget. A single writer may lose to N-1 contenders
        // on a hot stream, so the budget must comfortably exceed expected
        // concurrency; jittered backoff (below) breaks the thundering
        // herd so realistic contention converges well within it. Streams
        // hotter than this shouldn't use OCC — partition or queue them.
        const MAX_OCC_RETRIES: u32 = 16;
        let mut last_conflict: Option<anyhow::Error> = None;

        for attempt in 0..MAX_OCC_RETRIES {
            // Load: fold the PHYSICAL stream (SUBJECT) + capture
            // the expected stream state. `expected` is the stream head
            // across ALL co-located types (that's the OCC fence the
            // backend checks); the fold applies only `F`-typed events.
            let events = self.log.read_stream(F::SUBJECT, id, None).await?;
            let expected = match events.last() {
                None => StreamState::NoStream,
                Some(e) => StreamState::StreamRevision(e.revision.raw()),
            };
            let mut agg = A::default();
            for e in &events {
                if e.event_type != F::NAME {
                    continue; // foreign co-located type
                }
                let fact: F = serde_json::from_value(e.payload.clone())?;
                agg.apply(&fact);
            }

            // Decide (pure).
            let facts = decide(&agg)?;
            if facts.is_empty() {
                let position = self.log.latest_position().await?;
                return Ok(EmitResult { position, workflow_id: Uuid::new_v4() });
            }

            // Build the whole decision, then append it as one atomic
            // batch under OCC — the events land contiguously or not at all.
            let workflow = Uuid::new_v4();
            let mut events_data = Vec::with_capacity(facts.len());
            // (event_type, payload, event_id) for the post-append fold.
            let mut folds = Vec::with_capacity(facts.len());
            for fact in &facts {
                // The whole decision is written to the (F::NAME, id)
                // aggregate stream — every fact must belong to aggregate `id`.
                debug_assert_eq!(
                    fact.subject_id(), id,
                    "Engine::append: decided fact has subject_id {} but the \
                     command targets aggregate {id}; a decision may only emit \
                     facts for its own aggregate",
                    fact.subject_id(),
                );
                let event_type = crate::event::event_type_for(fact);
                let payload = serde_json::to_value(fact)?;
                let event = EventData {
                    event_id:        Uuid::new_v4(),
                    causation_id:    None,
                    workflow_id:  workflow,
                    event_type:      event_type.clone(),
                    payload:         payload.clone(),
                    created_at:      fact.occurred_at().unwrap_or_else(|| self.clock.now()),
                    // Placement uses SUBJECT (same as emit), so
                    // durable restore reads it back; routing stays on event_type.
                    category:        Some(F::SUBJECT.to_string()),
                    subject_id:       Some(id),
                    metadata:        self.default_metadata.clone(),
                    ephemeral:       None,
                    persistent:      true,
                };
                folds.push((event_type, payload, event.event_id));
                events_data.push(event);
            }

            match self.log.append_to_stream(F::SUBJECT, id, expected, events_data).await {
                Ok(result) => {
                    // Fold each fact into the engine registry so
                    // `engine.state_of::<A>(id).await.unwrap()` reflects the write. The
                    // batch committed atomically at one position;
                    // `result.revision` is the LAST fact's revision, so
                    // fact i sits at `result.revision - (n - 1 - i)`.
                    if let Some(reg) = &self.aggregators {
                        let n = folds.len() as u64;
                        for (i, (event_type, payload, event_id)) in folds.iter().enumerate() {
                            let revision = crate::types::StreamRevision::from_raw(
                                result.revision.raw() - (n - 1 - i as u64),
                            );
                            let outcome = crate::aggregator::fold_event(
                                reg.as_ref(),
                                self.snapshot_store.as_deref(),
                                self.log.as_ref(),
                                event_type,
                                payload,
                                id,
                                F::SUBJECT,
                                revision,
                                result.position,
                                /* strict_to_event = */ false,
                            )
                            .await?;
                            if outcome.applied {
                                if let Some(obs) = self.observer.as_ref() {
                                    reg.notify_observer(
                                        &outcome.snapshots, obs.as_ref(), workflow,
                                        result.position, *event_id,
                                    );
                                }
                            }
                        }
                    }
                    return Ok(EmitResult {
                        position: result.position,
                        workflow_id: workflow,
                    });
                }
                Err(e) => {
                    if e.downcast_ref::<crate::event_log::ConflictError>().is_some() {
                        last_conflict = Some(e);
                        // Jittered exponential backoff before reload +
                        // re-decide, so concurrent losers don't all retry
                        // in lockstep and collide again. Jitter entropy
                        // from a fresh UUID (no extra dependency). At
                        // command time, not replay — a non-deterministic
                        // delay never affects which events are written.
                        let window_us = 50u64 << attempt.min(6); // 50µs … ~3.2ms
                        let jitter_us = (Uuid::new_v4().as_u128() as u64) % window_us;
                        tokio::time::sleep(
                            std::time::Duration::from_micros(jitter_us),
                        ).await;
                        continue; // reload + re-decide against the new tail
                    }
                    return Err(e);
                }
            }
        }

        Err(last_conflict.unwrap_or_else(|| {
            anyhow::anyhow!(
                "Engine::append exhausted {MAX_OCC_RETRIES} OCC retries for \
                 '{}-{}'",
                F::NAME, id,
            )
        }))
    }

    async fn execute_emit(&self, b: EmitBuilder<'_>) -> Result<EmitResult> {
        // Empty batch is a successful no-op. Returns the log's
        // current latest position so a downstream `settled()`
        // waits for any pre-existing pending work to drain (rather
        // than returning trivially against `LogCursor::ZERO`).
        if b.input.facts.is_empty() {
            let position = self.log.latest_position().await?;
            let workflow_id = b.workflow_id.unwrap_or_else(Uuid::new_v4);
            return Ok(EmitResult { position, workflow_id });
        }

        // ── Workflow resolution. A fact type that declares
        // `workflow_id = "field"` is a workflow ROOT: its envelope
        // workflow comes from its own payload, constitutionally, at
        // every emit site. One source of truth — so a builder
        // `.workflow_id(x)` against a declaring type is a loud error,
        // never a silent precedence; and a batch can't mix roots with
        // members (or roots of different workflows): that's two
        // workflows wearing one emit — emit them separately.
        let declared: Vec<Option<Uuid>> =
            b.input.facts.iter().map(|f| f.declared_workflow_id()).collect();
        let workflow = if let Some(first_declared) = declared.iter().flatten().next() {
            if let Some(explicit) = b.workflow_id {
                anyhow::bail!(
                    "emit().workflow_id({explicit}) conflicts with a workflow-ROOT \
                     fact in this batch (its type declares its workflow from its \
                     own payload field, naming workflow {first_declared}). The \
                     declaration is the single source of truth — drop the builder \
                     override.",
                );
            }
            let first = *first_declared;
            if declared.iter().any(|d| *d != Some(first)) {
                anyhow::bail!(
                    "emit() batch mixes workflow-ROOT facts with chain members (or \
                     roots of different workflows). A batch shares one workflow; \
                     emit roots separately.",
                );
            }
            first
        } else {
            b.workflow_id.unwrap_or_else(Uuid::new_v4)
        };
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

        // ── Build + validate the WHOLE batch before any write. The OCC
        // guard and serialization run per fact up front, so a rejected
        // fact mid-batch can't tear the batch (the pre-2026-06 path
        // appended one fact at a time: facts before the rejected one
        // were already durable when emit returned Err).
        struct PendingEmit {
            subject: String,
            subject_id:       Uuid,
            event:           EventData,
        }
        let mut pending: Vec<PendingEmit> = Vec::with_capacity(b.input.facts.len());
        for fact in b.input.facts {
            // StreamPolicy::OccRequired guard. Facts in a category
            // registered via `with_aggregate` carry invariants and must
            // go through the optimistic-concurrency command path
            // `Engine::append`, not the no-check `emit` path.
            if self.occ_categories.contains(fact.name()) {
                return Err(anyhow::anyhow!(
                    "category '{}' is OCC-required (registered via \
                     with_aggregate); use Engine::append::<A, F>(id, decide) \
                     instead of emit()",
                    fact.name(),
                ));
            }
            // event_type carries the ROUTING category (consumer/aggregator
            // matching); `category`/placement uses the STREAM category. They
            // differ only when the event overrides `SUBJECT`.
            let event_type = fact.name().to_string();
            let subject = fact.subject().to_string();
            let occurred_at = fact.occurred_at().unwrap_or_else(|| self.clock.now());
            let subject_id = fact.subject_id();
            let payload = fact.to_value()?;
            let event = EventData {
                event_id:        Uuid::new_v4(),
                causation_id:    b.causation_id,
                workflow_id:  workflow,
                event_type,
                payload,
                created_at:      occurred_at,
                category:        Some(subject.clone()),
                subject_id:       Some(subject_id),
                metadata:        merged_metadata.clone(),
                ephemeral:       None,
                persistent:      true,
            };
            pending.push(PendingEmit { subject, subject_id, event });
        }

        // ── Append in runs of consecutive same-stream facts — each run
        // is ONE atomic `append_to_stream` batch. The common shapes
        // (single fact; multi-fact batch on one stream) land all-or-
        // nothing, so a failed emit leaves nothing behind and a caller
        // retry can't duplicate a torn prefix. A mixed-stream batch is
        // atomic per run, in input order — atomicity ACROSS streams is
        // not provided (the backend primitive is per-stream); see the
        // `emit` docs.
        //
        // `emit` writes with `StreamState::Any` (no concurrency
        // invariant — OCC-bearing streams are rejected above and go
        // through `Engine::append`). Idempotency rests on `event_id`.
        let mut i = 0;
        while i < pending.len() {
            let mut j = i + 1;
            while j < pending.len()
                && pending[j].subject == pending[i].subject
                && pending[j].subject_id == pending[i].subject_id
            {
                j += 1;
            }
            let subject = pending[i].subject.clone();
            let subject_id = pending[i].subject_id;
            let chunk: Vec<EventData> =
                pending[i..j].iter().map(|p| p.event.clone()).collect();
            let n = chunk.len() as u64;

            let result = self
                .log
                .append_to_stream(
                    &subject,
                    subject_id,
                    crate::types::StreamState::Any,
                    chunk,
                )
                .await?;
            last_position = result.position;

            // Fold each fact of the run into the engine registry. The
            // run committed atomically; `result.revision` is the LAST
            // fact's revision, so fact k sits at `revision - (n-1-k)`
            // (same math as `Engine::append`). `result.position` is the
            // last fact's position — used for every fold in the run,
            // mirroring `Engine::append`'s existing approximation.
            // (The pre-append durable restore this path used to do is
            // subsumed by `fold_event`'s gap repair: a cold aggregate's
            // first fold detects the revision gap and restores
            // read-through before folding.)
            for (k, p) in pending[i..j].iter().enumerate() {
                let agg_event_type = p.event.event_type.clone();
                let agg_payload = p.event.payload.clone();
                let event_id = p.event.event_id;
                let revision = StreamRevision::from_raw(
                    result.revision.raw() - (n - 1 - k as u64),
                );
                let fact_write = crate::types::WriteResult {
                    position: result.position,
                    revision,
                };

                // Mirror into the engine-level registry so out-of-band
                // `engine.state_of::<A>(subject_id).await.unwrap()` reads stay fresh.
                // Consumers maintain their OWN registry clones for
                // in-body `ctx.state_of` reads — independent state.
                // Idempotent on stream coordinates; gap repair orders
                // racing concurrent emits to the same aggregate stream.
                if let Some(reg) = &self.aggregators {
                    let outcome = crate::aggregator::fold_event(
                        reg.as_ref(),
                        self.snapshot_store.as_deref(),
                        self.log.as_ref(),
                        &agg_event_type,
                        &agg_payload,
                        subject_id,
                        &subject,
                        fact_write.revision,
                        fact_write.position,
                        /* strict_to_event = */ false,
                    )
                    .await?;
                    if outcome.applied {
                        if let Some(obs) = self.observer.as_ref() {
                            reg.notify_observer(
                                &outcome.snapshots,
                                obs.as_ref(),
                                workflow,
                                fact_write.position,
                                event_id,
                            );
                        }
                        if let Some(store) = self.snapshot_store.as_ref() {
                            crate::aggregator::maybe_save_snapshots(
                                reg.as_ref(),
                                store.as_ref(),
                                self.snapshot_every,
                                &outcome.snapshots,
                            )
                            .await;
                        }
                    }
                }
            }
            i = j;
        }
        Ok(EmitResult {
            position: last_position,
            workflow_id: workflow,
        })
    }

    /// Read a subject's current state, **restoring it from durable
    /// storage if it isn't already in memory**. This is the only
    /// out-of-band read: the sync racy peek (`engine.state_of`) was
    /// deleted in the 0.10 step-1 rename — every use it had is this,
    /// minus the race.
    ///
    /// If the aggregate isn't cached, this loads its snapshot (if a
    /// [`with_snapshot_store`](EngineBuilder::with_snapshot_store) is wired),
    /// replays the tail of its stream
    /// (`{`[`A::SUBJECT`](crate::aggregate::Aggregate::SUBJECT)`}-{id}`),
    /// folds, and caches the result. A snapshot blob that fails to deserialize
    /// self-heals (deleted, rebuilt from genesis).
    ///
    /// Returns `None` when the aggregate has no events and no snapshot (or when
    /// `A::SUBJECT` is unset, i.e. restore is disabled).
    ///
    /// For ops/tests/read paths outside a consumer — in a
    /// reactor/projector body, read via `ctx.state_of::<A>(id)` (the
    /// runner restores before folding).
    ///
    /// # Panics
    /// If no aggregator was ever registered for `A` (or no aggregators
    /// at all): an unregistered type would otherwise read as "no state
    /// yet" forever, hiding a configuration bug.
    pub async fn state_of<A>(&self, id: Uuid) -> Result<Option<A>>
    where
        A: crate::aggregate::Aggregate + Clone + serde::de::DeserializeOwned,
    {
        let Some(reg) = self.aggregators.as_ref() else {
            panic!(
                "engine.state_of::<{}>() called but no aggregators were \
                 registered with EngineBuilder::with_aggregators(...)",
                std::any::type_name::<A>(),
            )
        };
        reg.get_transition_arc::<A>(id); // panics if `A` was never registered
        let key = format!("{}:{}", <A as crate::aggregate::Aggregate>::NAME, id);
        // Restore only when a snapshot store is wired (without it, behavior is
        // unchanged — this is a peek into in-memory state).
        if !reg.has_state(&key) && self.snapshot_store.is_some() {
            crate::aggregator::restore_aggregate(
                reg.as_ref(),
                self.snapshot_store.as_deref(),
                self.log.as_ref(),
                <A as crate::aggregate::Aggregate>::NAME,
                <A as crate::aggregate::Aggregate>::SUBJECT,
                id,
            )
            .await?;
        }
        if !reg.has_state(&key) {
            return Ok(None);
        }
        let (_, curr) = reg.get_transition_arc::<A>(id);
        Ok(Some((*curr).clone()))
    }

    /// Wait until the causal chain of `result.workflow_id` has fully
    /// quiesced — every consumer caught up to the run's furthest event, every
    /// reactor output in that chain appended, no pending work remaining for
    /// *this run*. Other runs' concurrent traffic does not delay it.
    ///
    /// Algorithm (per-workflow high-water):
    ///
    ///   1. `hw` = the furthest `$all` position of any event in this run's
    ///      chain (reactor outputs inherit the trigger's `workflow_id`),
    ///      floored at `result.position` so we always wait for the trigger to
    ///      be observed.
    ///   2. Wait for every consumer cursor to reach `hw`.
    ///   3. Re-read `hw`. If unchanged, the run has drained — return Ok.
    ///      Otherwise a reactor appended a new chain event while we waited;
    ///      loop.
    ///
    /// Terminates for well-formed topologies: each input produces a bounded
    /// number of outputs, consumers catch up, and the run's high-water stops
    /// moving. Self-feedback reactors (output retriggers itself) are NOT
    /// well-formed (see [`Reactor`] doc) and loop forever here too.
    ///
    /// **Single-engine boundary.** The high-water is tracked in-process, so
    /// `settle` is correct only when this run's reactors execute in the *same*
    /// engine instance that called `settle` (the typical single-engine
    /// deployment). If you run multiple engine instances against one shared
    /// log, a run's reactor output produced on another instance is invisible
    /// here and `settle` may return early — that topology needs a
    /// backend-queried high-water instead.
    ///
    /// **Isolation (0.10 step 2).** `settle` asks each consumer's
    /// `drained(workflow, hw)` probe, not its global durable cursor.
    /// For reactors that probe is "ingestion scanned to `hw` AND no
    /// queued/in-flight trigger of this workflow remains" — a reactor
    /// wedged on an *unrelated* workflow's trigger no longer delays
    /// this one. The remaining coupling is the pending-work window
    /// (BLOCKING-3): a consumer whose window is saturated by wedged
    /// work stops ingesting, which does delay settle — visibly, by
    /// design, instead of unbounded redelivery debt.
    ///
    /// Reactors run asynchronously in supervisor tasks; bounded latency depends
    /// on consumer batch size + supervisor poll interval.
    pub async fn settle(&self, result: EmitResult) -> Result<()> {
        let wf = result.workflow_id;
        loop {
            // High-water = the furthest position any event in THIS run's chain
            // has reached. Floor it at the emit position so we always wait for
            // consumers to at least observe the trigger, even before any
            // reactor output has landed (or if the entry was never tracked).
            let hw = self
                .workflow_hw
                .lock()
                .unwrap()
                .get(&wf)
                .unwrap_or(result.position);

            // Wait for every consumer to drain THIS workflow to hw. An
            // output appended while we wait inherits this workflow_id
            // and bumps the tracker — the hw re-check below catches it.
            for consumer in &self.consumers {
                loop {
                    if consumer.drained(wf, hw).await? {
                        break;
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }

            // Fall back to the prior hw (not the floor) if the entry was evicted
            // mid-settle, so eviction can't spuriously regress the comparison.
            let hw2 = self.workflow_hw.lock().unwrap().get(&wf).unwrap_or(hw);
            if hw2 == hw {
                self.workflow_hw.lock().unwrap().forget(&wf);
                return Ok(());
            }
        }
    }

    /// Settle a workflow AND every workflow transitively rooted from
    /// inside it — child roots are discovered through causation links,
    /// which cross workflow boundaries.
    ///
    /// **Test/ops-only. Do not reach for this in production code.**
    /// A workflow root exists precisely so the parent does NOT wait
    /// for the child's lifecycle; production code that needs a child's
    /// result writes a *reaction to the child's completion fact*, not
    /// a wait. Using `settle_tree` to await a slow child locally
    /// rebuilds the settle-hostage problem this design removed. Its
    /// legitimate users are test harnesses awaiting full pipelines and
    /// operators draining a system.
    ///
    /// Termination: same contract as [`Engine::settle`] — well-formed
    /// topologies produce bounded trees. A re-entrant mailbox cycle
    /// (a child emitting facts that join an ancestor's workflow,
    /// which roots new children, …) loops here exactly as a
    /// self-feedback reactor loops `settle`.
    pub async fn settle_tree(&self, result: EmitResult) -> Result<()> {
        let mut settled: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        let mut queue: Vec<EmitResult> = vec![result];
        loop {
            while let Some(r) = queue.pop() {
                if !settled.insert(r.workflow_id) {
                    continue;
                }
                self.settle(r).await?;
            }
            // Rescan for roots caused by any settled workflow but not
            // yet settled themselves (a settle may have appended new
            // children; a mailbox fact may have re-entered a settled
            // workflow). Stable ⇒ done.
            let children = self.scan_child_roots(&settled).await?;
            if children.is_empty() {
                return Ok(());
            }
            queue.extend(children);
        }
    }

    /// Find workflow roots caused by events of any workflow in `parents`
    /// but not in `parents` themselves: scan the log for events whose
    /// `causation_id` belongs to a parent workflow while their own
    /// `workflow_id` differs. Returns one `EmitResult` per child
    /// (furthest position seen). Full-log scan — test/ops tooling.
    async fn scan_child_roots(
        &self,
        parents: &std::collections::HashSet<Uuid>,
    ) -> Result<Vec<EmitResult>> {
        let mut parent_events: std::collections::HashSet<Uuid> =
            std::collections::HashSet::new();
        let mut children: std::collections::HashMap<Uuid, LogCursor> =
            std::collections::HashMap::new();
        let mut from = LogCursor::ZERO;
        loop {
            let batch = self.log.read_all(from, 1024).await?;
            if batch.is_empty() {
                break;
            }
            from = batch.last().unwrap().position;
            for e in batch {
                if parents.contains(&e.workflow_id) {
                    parent_events.insert(e.event_id);
                    continue;
                }
                // Causation appears before its child in the log, so a
                // single forward pass sees parents first.
                if let Some(cause) = e.causation_id {
                    if parent_events.contains(&cause) {
                        let entry = children.entry(e.workflow_id).or_insert(e.position);
                        if e.position > *entry {
                            *entry = e.position;
                        }
                    }
                }
            }
        }
        Ok(children
            .into_iter()
            .map(|(workflow_id, position)| EmitResult { position, workflow_id })
            .collect())
    }

    /// A named entry point for genuinely exogenous signals — clock
    /// ticks, HTTP mutations, webhook callbacks. Emits made through it
    /// stamp `_origin = <name>` into the envelope metadata, so every
    /// fact's provenance is attributable: it either has a causation
    /// parent (in-spine) or an origin (boundary).
    ///
    /// Workflow-root declarations pay double here: a `RunRequested`
    /// declaring `workflow_id = "run_id"` roots the same workflow the
    /// same way whether it enters from GraphQL, an ops tool, or a
    /// scheduler — no builder discipline at any of them. A healthy
    /// app has very few boundaries.
    ///
    /// ```ignore
    /// engine.boundary("due_sweep")
    ///     .emit(SweepRequested { sweep_id, .. })
    ///     .await?;
    /// ```
    pub fn boundary<'a>(&'a self, name: &'a str) -> Boundary<'a> {
        Boundary { engine: self, name }
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

    /// Signal shutdown; drain in-flight consumer steps; halt. Reactor
    /// partition workers stop at their next loop boundary (an in-flight
    /// `react()` completes; it is not cancelled).
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        for consumer in &self.consumers {
            consumer.halt();
        }
        for handle in self.handles {
            let _ = handle.await;
        }
        Ok(())
    }
}

/// Named boundary handle — see [`Engine::boundary`]. Every emit made
/// through it carries `_origin = <name>` in the envelope metadata.
pub struct Boundary<'a> {
    engine: &'a Engine,
    name: &'a str,
}

impl<'a> Boundary<'a> {
    /// Emit through this boundary: a regular [`EmitBuilder`] with the
    /// origin already stamped. Chain `.metadata()` / `.settled()` /
    /// `.await` as usual.
    pub fn emit<I: Into<EmitInput>>(&self, input: I) -> EmitBuilder<'a> {
        self.engine.emit(input).metadata("_origin", self.name)
    }
}

async fn supervise_one(
    consumer: Arc<dyn Supervisable>,
    shutdown: &mut broadcast::Receiver<()>,
) {
    loop {
        if shutdown.try_recv().is_ok() { break; }

        // Catch panics from the consumer body so a misconfiguration
        // (e.g., ctx.state_of without aggregators registered) or a
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
                     Common cause: ctx.state_of called without aggregators \
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
pub(crate) fn panic_payload_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contexts::Ctx;
        use crate::memory_store::MemoryStore;
    use crate::reactor::Events;
    use chrono::DateTime;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct UserCreated {
        user_id:     Uuid,
        occurred_at: DateTime<Utc>,
    }
    impl Event for UserCreated {
        const NAME: &'static str = "user_created";
        fn subject_id(&self) -> Uuid { self.user_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct WelcomeQueued {
        user_id: Uuid,
    }
    impl Event for WelcomeQueued {
        const NAME: &'static str = "welcome_queued";
        fn subject_id(&self) -> Uuid { self.user_id }
    }

    /// Projector that records every user_id it sees.
    #[derive(Default, Clone)]
    struct UserRoster {
        seen: Arc<parking_lot::Mutex<Vec<Uuid>>>,
    }
    #[async_trait]
    impl Projector for UserRoster {
        type Event = UserCreated;
        const NAME: &'static str = "users";
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
        const NAME: &'static str = "welcome.reactor";
        async fn react(
            &self, trigger: &UserCreated, _ctx: Ctx<'_>,
        ) -> Result<Events> {
            let mut out = Events::new();
            out.push(WelcomeQueued { user_id: trigger.user_id });
            Ok(out)
        }
    }

    fn store() -> Arc<MemoryStore> { Arc::new(MemoryStore::new()) }

    /// B2: a freshly-deployed reactor against an EXISTING log must not
    /// re-fire side effects for history — its cursor seeds at
    /// `latest_position()` during `build()`. Events emitted after build
    /// process normally. `StartPosition::Zero` is the explicit opt-in
    /// for processing history.
    #[tokio::test]
    async fn fresh_reactor_seeds_at_latest_and_skips_history() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static FIRED: AtomicUsize = AtomicUsize::new(0);
        struct CountingReactor;
        #[async_trait]
        impl Reactor for CountingReactor {
            type Trigger = UserCreated;
            const NAME: &'static str = "seed.counting";
            async fn react(&self, _t: &UserCreated, _ctx: Ctx<'_>) -> Result<Events> {
                FIRED.fetch_add(1, Ordering::SeqCst);
                Ok(Events::new())
            }
        }

        let store = store();
        // Pre-existing history: an engine WITHOUT the reactor emits one.
        {
            let engine = EngineBuilder::new(
                store.clone() as Arc<dyn EventLogBackend>,
                store.clone() as Arc<dyn CheckpointStore>,
                store.clone() as Arc<dyn ReactorCheckpoint>,
            ).build().await.unwrap();
            engine.emit(UserCreated { user_id: Uuid::new_v4(), occurred_at: Utc::now() })
                .settled().await.unwrap();
            engine.shutdown().await.unwrap();
        }

        // "Deploy" the reactor: cursor seeds at latest during build,
        // so the historical trigger never reaches react().
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_reactor(CountingReactor)
        .build().await.unwrap();

        // A new trigger fires exactly once.
        engine.emit(UserCreated { user_id: Uuid::new_v4(), occurred_at: Utc::now() })
            .settled().await.unwrap();
        assert_eq!(FIRED.load(Ordering::SeqCst), 1,
                   "history skipped; only the post-deploy trigger fired");
        engine.shutdown().await.unwrap();
    }

    /// Flat routing made colons legal (no separator to protect), but
    /// an EMPTY kind is still a broken wire identity — `build()`
    /// rejects it loudly rather than registering an unmatchable fold.
    #[tokio::test]
    async fn build_rejects_empty_event_kind() {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct Weird { id: Uuid }
        impl Event for Weird {
            const NAME: &'static str = ""; // empty = invalid wire identity
            fn subject_id(&self) -> Uuid { self.id }
        }
        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct WeirdAgg { n: u32 }
        impl crate::aggregate::Aggregate for WeirdAgg {
            const NAME: &'static str = "WeirdAgg";
        }
        impl crate::aggregate::Apply<Weird> for WeirdAgg {
            fn apply(&mut self, _: &Weird) { self.n += 1; }
        }

        let store = store();
        let result = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![Aggregator::for_type::<WeirdAgg, Weird>()])
        .build()
        .await;
        let err = match result {
            Ok(_) => panic!("build must reject an empty event kind"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("empty"), "got: {err}");
    }

    /// B2: `StartPosition::Zero` forces the reactor through history —
    /// the explicit replay opt-in.
    #[tokio::test]
    async fn with_reactor_start_zero_processes_history() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static FIRED_ZERO: AtomicUsize = AtomicUsize::new(0);
        struct CountingReactorZero;
        #[async_trait]
        impl Reactor for CountingReactorZero {
            type Trigger = UserCreated;
            const NAME: &'static str = "seed.zero";
            async fn react(&self, _t: &UserCreated, _ctx: Ctx<'_>) -> Result<Events> {
                FIRED_ZERO.fetch_add(1, Ordering::SeqCst);
                Ok(Events::new())
            }
        }

        let store = store();
        {
            let engine = EngineBuilder::new(
                store.clone() as Arc<dyn EventLogBackend>,
                store.clone() as Arc<dyn CheckpointStore>,
                store.clone() as Arc<dyn ReactorCheckpoint>,
            ).build().await.unwrap();
            engine.emit(UserCreated { user_id: Uuid::new_v4(), occurred_at: Utc::now() })
                .settled().await.unwrap();
            engine.shutdown().await.unwrap();
        }

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_reactor_start(CountingReactorZero, crate::projection::StartPosition::Zero)
        .build().await.unwrap();

        engine.emit(UserCreated { user_id: Uuid::new_v4(), occurred_at: Utc::now() })
            .settled().await.unwrap();
        assert_eq!(FIRED_ZERO.load(Ordering::SeqCst), 2,
                   "Zero opt-in replays the historical trigger AND fires the new one");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_drives_projector_end_to_end() {
        let store = store();
        let roster = UserRoster::default();
        let seen = roster.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projector(roster)
        .build().await.unwrap();

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
    async fn engine_drives_reactor_chain_end_to_end() {
        let store = store();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_assertion = counter.clone();

        // A second projector that observes the WelcomeQueued facts
        // emitted by the reactor — verifies the full chain:
        //   emit UserCreated → reactor appends WelcomeQueued to the log
        //   → second projector sees WelcomeQueued.
        struct WelcomeCounter(Arc<AtomicUsize>);
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct WelcomeQueuedFact { user_id: Uuid }
        impl Event for WelcomeQueuedFact {
            const NAME: &'static str = "welcome_queued";
            fn subject_id(&self) -> Uuid { self.user_id }
        }
        #[async_trait]
        impl Projector for WelcomeCounter {
            type Event = WelcomeQueuedFact;
            const NAME: &'static str = "welcome.counter";
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_reactor(WelcomeReactor)
        .with_projector(WelcomeCounter(counter))
        .build().await.unwrap();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        // Wait for the reactor → projector chain.
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

    /// Regression test for the reactor-append → engine-aggregator fold path.
    ///
    /// Before this path existed, `engine.state_of::<A>(subject_id).await.unwrap()`
    /// reflected only caller-emitted facts; reactor-emitted facts
    /// updated each consumer's private registry clone but were
    /// invisible to out-of-band readers. That made the saga-style
    /// "one aggregate, many fact types" pattern unusable from tests —
    /// you could emit a trigger and verify its direct effect on state,
    /// but not the effect of downstream reactor outputs.
    ///
    /// The fix folds every reactor output into the engine aggregator
    /// registry right after it's appended to the log. This test pins
    /// that contract: emit a UserCreated, the reactor emits a
    /// WelcomeQueued, settle, then `engine.state_of::<ChainCount>` on
    /// the user_id stream reflects BOTH (one UserCreated, one
    /// WelcomeQueued — total = 2).
    ///
    /// If a future refactor of the reactor runner omits the `apply_event`
    /// call (or wires it to the wrong registry), this assertion drops to 1.
    #[tokio::test]
    async fn engine_snapshot_sees_reactor_emitted_facts() {
        use crate::aggregate::{Aggregate, Apply};

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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([
            Aggregator::for_type::<ChainCount, UserCreated>(),
            Aggregator::for_type::<ChainCount, WelcomeQueued>(),
        ])
        .with_reactor(WelcomeReactor)
        .build().await.unwrap();

        let user_id = Uuid::new_v4();
        engine.emit(UserCreated {
            user_id,
            occurred_at: Utc::now(),
        }).settled().await.unwrap();

        let state: ChainCount = engine
            .state_of::<ChainCount>(user_id).await.unwrap()
            .expect("snapshot must exist after settled emit");
        assert_eq!(state.applied, 2,
            "engine.snapshot must reflect BOTH caller-emitted UserCreated \
             AND reactor-emitted WelcomeQueued — reactor-side fold contract");

        engine.shutdown().await.unwrap();
    }

    /// Concurrency contract of `AggregatorRegistry::apply_event`:
    /// the RMW lives under a single `state.entry(key)` guard (per-shard
    /// locking serializes concurrent applies on the same key — the
    /// 0.4.4 lost-update fix), and folds are **idempotent on stream
    /// coordinates** (the 2026-06-10 A2 fix): N racing redeliveries of
    /// the same event fold exactly once, and a dense sequence of
    /// revisions folds exactly once each.
    #[test]
    fn aggregator_apply_event_serializes_concurrent_callers() {
        use crate::aggregate::{Aggregate, Apply};

        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct Counter {
            n: u32,
        }
        impl Aggregate for Counter {
            const NAME: &'static str = "Counter";
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct Inc {
            subject_id: Uuid,
        }
        impl Event for Inc {
            const NAME: &'static str = "inc";
            fn subject_id(&self) -> Uuid {
                self.subject_id
            }
        }
        impl Apply<Inc> for Counter {
            fn apply(&mut self, _: &Inc) {
                self.n += 1;
            }
        }

        let mut reg = crate::aggregator::AggregatorRegistry::new();
        reg.register(crate::aggregator::Aggregator::for_type::<Counter, Inc>());
        let reg = Arc::new(reg);

        let subject_id = Uuid::new_v4();
        let event_type = "inc";
        let payload = serde_json::to_value(&Inc { subject_id }).unwrap();

        const TASKS: usize = 8;
        const PER_TASK: usize = 200;
        const REVISIONS: u64 = 50;

        // Every thread redelivers the SAME dense revision sequence.
        // 8 × 200 racing applies per revision must fold exactly once
        // per revision: the entry guard serializes the RMW and the
        // revision gate makes redelivery a no-op.
        let mut handles = Vec::new();
        for _ in 0..TASKS {
            let reg = reg.clone();
            let payload = payload.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_TASK {
                    for rev in 0..REVISIONS {
                        reg.apply_event(
                            event_type,
                            &payload,
                            subject_id,
                            "race",
                            crate::types::StreamRevision::from_raw(rev),
                            LogCursor::from_raw(rev + 1),
                        )
                        .expect("fold must not error");
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("thread joined");
        }

        let key = format!("Counter:{}", subject_id);
        let version = reg.get_version(&key);
        assert_eq!(
            version.raw(),
            REVISIONS,
            "watermark must sit one past the last folded revision"
        );

        let state = reg.get_state(&key).expect("state must exist");
        let counter = state
            .downcast_ref::<Counter>()
            .expect("type-erased state downcasts to Counter");
        assert_eq!(
            counter.n as u64,
            REVISIONS,
            "each revision folds exactly once — redeliveries skip, none lost"
        );
    }

    /// Pin the `Aggregator::for_type_with_id_fn` contract: a single
    /// fact type can register two aggregators with different keys.
    ///
    /// Before 0.4.5: the `#[aggregator(id_fn = "...")]` macro accepted
    /// the attribute but the factory hard-coded `Event::subject_id`, so
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
        use crate::aggregate::{Aggregate, Apply};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct OrgUserCreated {
            user_id: Uuid,
            org_id: Uuid,
            occurred_at: DateTime<Utc>,
        }
        impl Event for OrgUserCreated {
            const NAME: &'static str = "org_user_created";
            fn subject_id(&self) -> Uuid { self.user_id }
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([
            // UserCount keys by the natural subject_id (user_id).
            Aggregator::for_type::<UserCount, OrgUserCreated>(),
            // OrgCount keys by org_id — a different field. Pre-0.4.5
            // this was impossible; the factory ignored id_fn and
            // also folded into user_id, collapsing both aggregators
            // onto the same key.
            Aggregator::for_type_with_id_fn::<OrgCount, OrgUserCreated, _>(
                |e: &OrgUserCreated| Some(e.org_id)
            ),
        ])
        .build().await.unwrap();

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
        assert_eq!(engine.state_of::<UserCount>(user_1).await.unwrap().unwrap().n, 1);
        assert_eq!(engine.state_of::<UserCount>(user_2).await.unwrap().unwrap().n, 1);
        assert_eq!(engine.state_of::<UserCount>(user_3).await.unwrap().unwrap().n, 1);

        // OrgCount: keyed by org_id → 2 for org_a, 1 for org_b.
        assert_eq!(engine.state_of::<OrgCount>(org_a).await.unwrap().unwrap().n, 2,
            "id_fn must extract org_id, not user_id (Event::subject_id)");
        assert_eq!(engine.state_of::<OrgCount>(org_b).await.unwrap().unwrap().n, 1);

        // Cross-check: org_a snapshot at user_1's key must be None
        // (the OrgCount aggregator never folded there).
        assert!(engine.state_of::<OrgCount>(user_1).await.unwrap().is_none(),
            "OrgCount keyed by org_id must not appear under user_id key");

        engine.shutdown().await.unwrap();
    }

    /// `id_fn` that returns `Option<Uuid>` and yields `None` for some
    /// facts must skip the fold entirely (not fold at `Uuid::nil`).
    #[tokio::test]
    async fn aggregator_id_fn_returning_none_skips_fold() {
        use crate::aggregate::{Aggregate, Apply};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct MaybeRunEvent {
            subject_id: Uuid,
            run_id: Option<Uuid>,
            occurred_at: DateTime<Utc>,
        }
        impl Event for MaybeRunEvent {
            const NAME: &'static str = "maybe_run_event";
            fn subject_id(&self) -> Uuid { self.subject_id }
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([
            Aggregator::for_type_with_id_fn::<RunCounter, MaybeRunEvent, _>(
                |e: &MaybeRunEvent| e.run_id
            ),
        ])
        .build().await.unwrap();

        let run = Uuid::new_v4();

        // Two events with run_id, one without.
        engine.emit(MaybeRunEvent { subject_id: Uuid::new_v4(), run_id: Some(run), occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(MaybeRunEvent { subject_id: Uuid::new_v4(), run_id: None, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(MaybeRunEvent { subject_id: Uuid::new_v4(), run_id: Some(run), occurred_at: Utc::now() }).settled().await.unwrap();

        assert_eq!(engine.state_of::<RunCounter>(run).await.unwrap().unwrap().n, 2,
            "only the two facts with run_id Some should fold");

        // The None-run fact must not have created an entry at
        // Uuid::nil — verify nothing leaked there.
        assert!(engine.state_of::<RunCounter>(Uuid::nil()).await.unwrap().is_none(),
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
    impl Event for TaggedEvent {
        const NAME: &'static str = "tagged";
        // subject_id intentionally NOT tag_id — proves the macro
        // uses id_fn over subject_id.
        fn subject_id(&self) -> Uuid { self.event_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }
    impl TaggedEvent {
        fn tag(&self) -> Uuid { self.tag_id }
    }

    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct TagBucket { count: u32 }
    impl crate::aggregate::Aggregate for TagBucket {
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
    /// by `Event::subject_id`. This is the contract the scout side
    /// depends on (e.g. SignalLifecycle keyed by signal_id from a
    /// CuriosityEvent whose subject_id is nil).
    ///
    /// Regression: the macro must thread the `id_fn` attribute
    /// through to the factory rather than hard-coding `Event::subject_id`.
    #[tokio::test]
    async fn macro_aggregator_id_fn_actually_keys_by_method() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(tagged_aggs::aggregators())
        .build().await.unwrap();

        let tag_a = Uuid::new_v4();
        let tag_b = Uuid::new_v4();

        engine.emit(TaggedEvent { event_id: Uuid::new_v4(), tag_id: tag_a, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(TaggedEvent { event_id: Uuid::new_v4(), tag_id: tag_a, occurred_at: Utc::now() }).settled().await.unwrap();
        engine.emit(TaggedEvent { event_id: Uuid::new_v4(), tag_id: tag_b, occurred_at: Utc::now() }).settled().await.unwrap();

        assert_eq!(engine.state_of::<TagBucket>(tag_a).await.unwrap().unwrap().count, 2,
            "#[aggregator(id_fn = \"tag\")] must key by tag_id, not event_id");
        assert_eq!(engine.state_of::<TagBucket>(tag_b).await.unwrap().unwrap().count, 1);

        engine.shutdown().await.unwrap();
    }

    // ── Phase 5a — Aggregate-stream emit gating ──

    /// Aggregate registered for stream-policy testing.
    #[derive(Default, Clone, Serialize, Deserialize)]
    struct UserAgg;
    impl crate::aggregate::Aggregate for UserAgg {
        const NAME: &'static str = "UserAgg";
    }
    impl crate::aggregate::Apply<UserCreated> for UserAgg {
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).build().await.unwrap();

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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .build().await.unwrap();

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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .build().await.unwrap();

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct OrderPlaced { order_id: Uuid, occurred_at: DateTime<Utc> }
        impl Event for OrderPlaced {
            const NAME: &'static str = "order_placed";
            fn subject_id(&self) -> Uuid { self.order_id }
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
    impl Event for CounterFact {
        const NAME: &'static str = "counter";
        fn subject_id(&self) -> Uuid {
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
    impl crate::aggregate::Aggregate for Counter {
        const NAME: &'static str = "Counter";
    }

    /// The INVARIANT twin: same fold, fenced fact kind. Used by the
    /// OCC tests; `Counter` stays unfenced so emit-path tests can
    /// write CounterFact freely.
    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct GuardedCounter { value: i32 }
    impl crate::aggregate::Aggregate for GuardedCounter {
        const NAME: &'static str = "GuardedCounter";
        const INVARIANT: bool = true;
    }
    impl crate::aggregate::Apply<CounterFact> for GuardedCounter {
        fn apply(&mut self, fact: &CounterFact) {
            match fact {
                CounterFact::Inc { by, .. } => self.value += by,
                CounterFact::Reset { .. }   => self.value = 0,
            }
        }
    }
    impl crate::aggregate::Apply<CounterFact> for Counter {
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build().await.unwrap();

        let id = Uuid::new_v4();
        let (agg, ver) = engine.load::<GuardedCounter, CounterFact>(id).await.unwrap();
        assert_eq!(agg.value, GuardedCounter::default().value);
        assert_eq!(ver, StreamRevision::ZERO);
        engine.shutdown().await.unwrap();
    }

    // ── C3 — decider keys streams on SUBJECT, skips foreign types ──

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Deposited { account: Uuid, amount: i64 }
    impl Event for Deposited {
        const NAME: &'static str = "deposited";
        const SUBJECT: &'static str = "account"; // co-located
        fn subject_id(&self) -> Uuid { self.account }
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Withdrawn { account: Uuid, amount: i64 }
    impl Event for Withdrawn {
        const NAME: &'static str = "withdrawn";
        const SUBJECT: &'static str = "account"; // co-located
        fn subject_id(&self) -> Uuid { self.account }
    }
    #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Balance { value: i64 }
    impl crate::aggregate::Aggregate for Balance {
        const NAME: &'static str = "Balance";
        const SUBJECT: &'static str = "account";
    }
    impl crate::aggregate::Apply<Deposited> for Balance {
        fn apply(&mut self, d: &Deposited) { self.value += d.amount; }
    }
    impl crate::aggregate::Apply<Withdrawn> for Balance {
        fn apply(&mut self, w: &Withdrawn) { self.value -= w.amount; }
    }

    #[tokio::test]
    async fn decider_keys_on_subject_and_skips_foreign_types() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([
            Aggregator::for_type::<Balance, Deposited>(),
            Aggregator::for_type::<Balance, Withdrawn>(),
        ])
        .build().await.unwrap();

        let account = Uuid::new_v4();
        // Two co-located event types land in ONE stream ("account").
        engine.emit(Deposited { account, amount: 100 }).settled().await.unwrap();
        engine.emit(Withdrawn { account, amount: 30 }).settled().await.unwrap();

        // Both physically live in `account-<id>`, NOT in `deposit-`/`withdraw-`.
        let account_stream = EventLogBackend::read_stream(store.as_ref(), "account", account, None)
            .await.unwrap();
        assert_eq!(account_stream.len(), 2, "co-located in the account stream");
        let deposit_stream = EventLogBackend::read_stream(store.as_ref(), "deposit", account, None)
            .await.unwrap();
        assert!(deposit_stream.is_empty(),
                "C3: events must NOT be placed under their CATEGORY when SUBJECT differs");

        // load::<Balance, Deposited> reads the `account` stream, folds ONLY
        // Deposited (skips the co-located Withdrawn without crashing on a
        // cross-type deserialize), and reports the stream head revision.
        let (bal, rev) = engine.load::<Balance, Deposited>(account).await.unwrap();
        assert_eq!(bal.value, 100, "folded only the Deposited, skipped the Withdrawn");
        assert_eq!(rev, StreamRevision::from_raw(1), "revision is the stream head (2 events)");

        // append::<Balance, Deposited> writes to the `account` stream too.
        engine.append::<Balance, Deposited, _>(account, move |_b| {
            Ok(vec![Deposited { account, amount: 5 }])
        }).await.unwrap();
        let account_stream = EventLogBackend::read_stream(store.as_ref(), "account", account, None)
            .await.unwrap();
        assert_eq!(account_stream.len(), 3, "the decided fact landed in the account stream");

        engine.shutdown().await.unwrap();
    }

    // ── C4 — OCC fence: reactors cannot emit into OCC-required categories ──

    #[tokio::test]
    async fn reactor_emitting_into_occ_category_is_terminal_failured_not_appended() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A reactor that (wrongly) emits a CounterFact — whose category
        // "counter" is OCC-required (registered via with_aggregate).
        struct EmitsIntoOcc;
        #[async_trait]
        impl Reactor for EmitsIntoOcc {
            type Trigger = UserCreated;
            const NAME: &'static str = "occ.fence";
            async fn react(&self, t: &UserCreated, _: Ctx<'_>) -> Result<Events> {
                Ok(causal::events![CounterFact::Inc {
                    by: 1,
                    occurred_at: t.occurred_at,
                    counter_id: t.user_id,
                }])
            }
        }

        let store = store();
        let terminal_failure_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let terminal_failure_hits_c = terminal_failure_hits.clone();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        // GuardedCounter::INVARIANT = true ⇒ "counter" facts are fenced —
        // ordinary registration installs the fence.
        .with_aggregators([Aggregator::for_type::<GuardedCounter, CounterFact>()])
        .on_terminal_failure(move |info: TerminalFailure| {
            terminal_failure_hits_c.fetch_add(1, Ordering::SeqCst);
            assert!(info.error.contains("OCC-required"), "got: {}", info.error);
            None::<CounterFact> // no synthesized fact
        })
        .with_max_attempts(1)
        .with_reactor(EmitsIntoOcc)
        .build().await.unwrap();

        engine.emit(UserCreated { user_id: Uuid::new_v4(), occurred_at: Utc::now() })
            .settled().await.unwrap();

        // Give the reactor a moment to process + terminal-failure.
        for _ in 0..50 {
            if terminal_failure_hits.load(Ordering::SeqCst) > 0 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(terminal_failure_hits.load(Ordering::SeqCst), 1,
                   "the OCC-fence violation was routed to the failure store");

        // The CounterFact never landed in the "counter" stream.
        let counter_stream = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 100)
            .await.unwrap();
        assert!(
            counter_stream.iter().all(|e| !e.event_type.starts_with("counter:")),
            "the reactor's OCC-category output must NOT have been appended",
        );
        engine.shutdown().await.unwrap();
    }

    // ── Phase 3 — OCC command path (Engine::append) ──

    async fn occ_engine(store: &Arc<MemoryStore>) -> Engine {
        EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<GuardedCounter, CounterFact>()])
        .build()
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn append_persists_and_folds_across_calls() {
        let store = store();
        let engine = occ_engine(&store).await;
        let id = Uuid::new_v4();

        engine
            .append::<GuardedCounter, CounterFact, _>(id, move |_c| {
                Ok(vec![CounterFact::Inc { by: 3, occurred_at: Utc::now(), counter_id: id }])
            })
            .await
            .unwrap();
        // Second decision folds the first append's state.
        engine
            .append::<GuardedCounter, CounterFact, _>(id, move |c| {
                assert_eq!(c.value, 3, "decide sees the prior append folded in");
                Ok(vec![CounterFact::Inc { by: 7, occurred_at: Utc::now(), counter_id: id }])
            })
            .await
            .unwrap();

        let (c, ver) = engine.load::<GuardedCounter, CounterFact>(id).await.unwrap();
        assert_eq!(c.value, 10);
        assert_eq!(ver, StreamRevision::from_raw(1), "two events → tail revision 1");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn append_empty_decision_is_noop() {
        let store = store();
        let engine = occ_engine(&store).await;
        let id = Uuid::new_v4();

        engine
            .append::<GuardedCounter, CounterFact, _>(id, |_c| Ok(Vec::new()))
            .await
            .unwrap();

        let (c, _) = engine.load::<GuardedCounter, CounterFact>(id).await.unwrap();
        assert_eq!(c.value, 0, "empty decision wrote nothing");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_rejects_occ_required_category() {
        let store = store();
        let engine = occ_engine(&store).await;

        let err = engine
            .emit(CounterFact::Inc {
                by: 1,
                occurred_at: Utc::now(),
                counter_id: Uuid::new_v4(),
            })
            .await
            .expect_err("emit into an OCC-required category must error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("OCC-required") && msg.contains("counter"),
            "error must steer to Engine::append, got: {msg}"
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_occ_prevents_double_apply_under_concurrency() {
        // An "increment-once" guard: emit Inc{1} only if value == 0.
        // Without OCC, two concurrent appends both read 0 and both
        // apply → value 2 (guard violated). With OCC, the loser
        // conflicts, reloads, sees value 1, and no-ops → value 1.
        let store = store();
        let engine = Arc::new(occ_engine(&store).await);
        let id = Uuid::new_v4();

        let guard = move |c: &GuardedCounter| -> Result<Vec<CounterFact>> {
            if c.value == 0 {
                Ok(vec![CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id }])
            } else {
                Ok(Vec::new())
            }
        };

        let (e1, e2) = (engine.clone(), engine.clone());
        let t1 = tokio::spawn(async move { e1.append::<GuardedCounter, CounterFact, _>(id, guard).await });
        let t2 = tokio::spawn(async move { e2.append::<GuardedCounter, CounterFact, _>(id, guard).await });
        t1.await.unwrap().unwrap();
        t2.await.unwrap().unwrap();

        let (c, _) = engine.load::<GuardedCounter, CounterFact>(id).await.unwrap();
        assert_eq!(c.value, 1, "OCC prevented the double-apply (would be 2 without it)");

        Arc::try_unwrap(engine)
            .unwrap_or_else(|_| panic!("engine still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn append_multi_fact_decision_lands_contiguously() {
        let store = store();
        let engine = occ_engine(&store).await;
        let id = Uuid::new_v4();

        engine
            .append::<GuardedCounter, CounterFact, _>(id, move |_c| {
                Ok(vec![
                    CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
                    CounterFact::Inc { by: 2, occurred_at: Utc::now(), counter_id: id },
                    CounterFact::Inc { by: 4, occurred_at: Utc::now(), counter_id: id },
                ])
            })
            .await
            .unwrap();

        let events = EventLogBackend::read_stream(store.as_ref(), "counter", id, None)
            .await
            .unwrap();
        assert_eq!(events.len(), 3, "all three facts landed");
        assert_eq!(events[0].revision, StreamRevision::from_raw(0));
        assert_eq!(events[1].revision, StreamRevision::from_raw(1));
        assert_eq!(events[2].revision, StreamRevision::from_raw(2));

        let (c, _) = engine.load::<GuardedCounter, CounterFact>(id).await.unwrap();
        assert_eq!(c.value, 7);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn append_high_contention_all_increments_apply() {
        // Stress: N concurrent UNCONDITIONAL increments to ONE stream.
        // Each must land (no lost updates), so each contender may have
        // to retry up to N-1 times — the real test of the OCC retry
        // budget + backoff.
        const N: i32 = 8;
        let store = store();
        let engine = Arc::new(occ_engine(&store).await);
        let id = Uuid::new_v4();

        let mut tasks = Vec::new();
        for _ in 0..N {
            let e = engine.clone();
            tasks.push(tokio::spawn(async move {
                e.append::<GuardedCounter, CounterFact, _>(id, move |_c| {
                    Ok(vec![CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id }])
                })
                .await
            }));
        }
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap().is_ok() {
                ok += 1;
            }
        }

        let (c, _) = engine.load::<GuardedCounter, CounterFact>(id).await.unwrap();
        assert_eq!(ok, N, "all {N} appends succeeded (none exhausted OCC retries)");
        assert_eq!(c.value, N, "every increment applied — no lost update");

        Arc::try_unwrap(engine)
            .unwrap_or_else(|_| panic!("engine still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    // ── Phase 4 — EffectStore wired into the reactor path ──

    #[tokio::test]
    async fn effect_store_dedups_side_effect_across_retry() {
        use crate::effect_store::{InMemoryEffectStore, EffectStore};
        use std::sync::atomic::{AtomicU32, Ordering};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct Ping { id: Uuid, occurred_at: DateTime<Utc> }
        impl Event for Ping {
            const NAME: &'static str = "ping";
            fn subject_id(&self) -> Uuid { self.id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct Pong { id: Uuid, value: i64, occurred_at: DateTime<Utc> }
        impl Event for Pong {
            const NAME: &'static str = "pong";
            fn subject_id(&self) -> Uuid { self.id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        struct CachedSideEffect {
            external_calls: Arc<AtomicU32>,
            attempts: Arc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl Reactor for CachedSideEffect {
            type Trigger = Ping;
            const NAME: &'static str = "cached_side_effect";
            async fn react(&self, trigger: &Ping, ctx: Ctx<'_>) -> anyhow::Result<Events> {
                // The "expensive external call" — memoized by reaction key.
                let calls = self.external_calls.clone();
                let value: i64 = ctx
                    .effect("external_call", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(42)
                    })
                    .await?;

                // Fail the FIRST attempt (after the cached call) to force a
                // retry; the retry must NOT re-run the external call.
                if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("transient failure — forces a retry");
                }

                let mut out = Events::new();
                out.push(Pong { id: trigger.id, value, occurred_at: ctx.time() });
                Ok(out)
            }
        }

        let store = store();
        let external_calls = Arc::new(AtomicU32::new(0));
        let attempts = Arc::new(AtomicU32::new(0));
        let cache = Arc::new(InMemoryEffectStore::new());

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_effect_store(cache.clone() as Arc<dyn EffectStore>)
        .with_reactor(CachedSideEffect {
            external_calls: external_calls.clone(),
            attempts: attempts.clone(),
        })
        .build().await.unwrap();

        engine
            .emit(Ping { id: Uuid::new_v4(), occurred_at: Utc::now() })
            .settled()
            .await
            .unwrap();

        assert!(
            attempts.load(Ordering::SeqCst) >= 2,
            "reactor retried after the forced failure (attempts: {})",
            attempts.load(Ordering::SeqCst),
        );
        assert_eq!(
            external_calls.load(Ordering::SeqCst),
            1,
            "external call ran once despite the retry — EffectStore deduped it",
        );
        engine.shutdown().await.unwrap();
    }

    async fn batch_emit_atomicity_fixture(
        fail_on: usize,
    ) -> (Arc<MemoryStore>, Engine) {
        let mem = Arc::new(MemoryStore::new());
        let flaky = Arc::new(FailsNthAppend {
            inner: mem.clone(),
            calls: AtomicUsize::new(0),
            fail_on,
        });
        let engine = EngineBuilder::new(
            flaky as Arc<dyn EventLogBackend>,
            mem.clone() as Arc<dyn CheckpointStore>,
            mem.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .build().await.unwrap();
        (mem, engine)
    }

    /// Log wrapper that fails its Nth `append_to_stream` call (1-based),
    /// delegating everything else to the inner MemoryStore. Fault
    /// injection for batch-atomicity tests.
    struct FailsNthAppend {
        inner:   Arc<MemoryStore>,
        calls:   AtomicUsize,
        fail_on: usize,
    }
    #[async_trait]
    impl EventLogBackend for FailsNthAppend {
        async fn read_all(
            &self, after: LogCursor, limit: usize,
        ) -> Result<Vec<crate::types::RecordedEvent>> {
            EventLogBackend::read_all(self.inner.as_ref(), after, limit).await
        }
        async fn read_stream(
            &self, category: &str, subject_id: Uuid, after: Option<StreamRevision>,
        ) -> Result<Vec<crate::types::RecordedEvent>> {
            EventLogBackend::read_stream(self.inner.as_ref(), category, subject_id, after).await
        }
        async fn latest_position(&self) -> Result<LogCursor> {
            self.inner.latest_position().await
        }
        async fn append_to_stream(
            &self,
            category: &str,
            subject_id: Uuid,
            expected: crate::types::StreamState,
            events: Vec<EventData>,
        ) -> Result<crate::types::WriteResult> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_on {
                anyhow::bail!("injected append failure (call {n})");
            }
            self.inner.append_to_stream(category, subject_id, expected, events).await
        }
    }

    #[tokio::test]
    async fn same_stream_batch_emit_never_tears() {
        // A multi-fact emit to ONE stream must land all-or-nothing.
        // The torn outcome — emit returns Err but a prefix of the batch
        // is durably in the log — is the bug: the caller can neither
        // trust the Err (something landed) nor retry it (the retried
        // prefix duplicates under fresh event_ids).
        let (mem, engine) = batch_emit_atomicity_fixture(2).await;

        let id = Uuid::new_v4();
        let result = engine.emit(vec![
            CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
            CounterFact::Inc { by: 2, occurred_at: Utc::now(), counter_id: id },
        ]).await;

        let rows = EventLogBackend::read_all(mem.as_ref(), LogCursor::ZERO, 100)
            .await.unwrap();
        match result {
            Ok(_) => assert_eq!(rows.len(), 2, "Ok ⇒ the whole batch landed"),
            Err(_) => assert!(
                rows.is_empty(),
                "Err ⇒ nothing landed — got a TORN batch of {} row(s)",
                rows.len(),
            ),
        }
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn same_stream_batch_emit_retry_after_error_does_not_duplicate() {
        // Retry-after-Err is the natural caller response to a failed
        // emit. emit mints fresh event_ids per call (no cross-call
        // dedup), so retry safety rests ENTIRELY on failed emits being
        // all-or-nothing: if a failed attempt left a prefix behind, the
        // successful retry duplicates that prefix.
        let (mem, engine) = batch_emit_atomicity_fixture(2).await;

        let id = Uuid::new_v4();
        let mut succeeded = false;
        for _ in 0..3 {
            let r = engine.emit(vec![
                CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
                CounterFact::Inc { by: 2, occurred_at: Utc::now(), counter_id: id },
            ]).await;
            if r.is_ok() { succeeded = true; break; }
        }
        assert!(succeeded, "emit must eventually succeed past the injected fault");

        let rows = EventLogBackend::read_all(mem.as_ref(), LogCursor::ZERO, 100)
            .await.unwrap();
        let by_1 = rows.iter().filter(|e| e.payload["Inc"]["by"] == 1).count();
        let by_2 = rows.iter().filter(|e| e.payload["Inc"]["by"] == 2).count();
        assert_eq!((by_1, by_2), (1, 1),
                   "each fact exactly once after retry; rows: {:?}",
                   rows.iter().map(|e| &e.payload).collect::<Vec<_>>());
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_batch_round_trips_through_aggregator_fold() {
        // Emitting a batch of facts of the registered Event type
        // folds them into the engine's aggregator state, readable
        // via the read-side hydration helper.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build().await.unwrap();

        let id = Uuid::new_v4();
        engine.emit(vec![
            CounterFact::Inc { by: 3, occurred_at: Utc::now(), counter_id: id },
            CounterFact::Inc { by: 5, occurred_at: Utc::now(), counter_id: id },
        ]).await.unwrap();

        let (agg, _) = engine.load::<GuardedCounter, CounterFact>(id).await.unwrap();
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build().await.unwrap();

        let id = Uuid::new_v4();
        let pinned = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap().with_timezone(&Utc);
        engine.emit(vec![
            CounterFact::Inc { by: 1, occurred_at: pinned, counter_id: id },
        ]).await.unwrap();

        let events = EventLogBackend::read_stream(
            store.as_ref(), <CounterFact as Event>::NAME, id, None,
        ).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].created_at, pinned,
                   "persisted created_at == fact.occurred_at()");

        engine.shutdown().await.unwrap();
    }

    // ── MultiProjector engine integration ──

    #[tokio::test]
    async fn engine_drives_multi_projector_seeing_heterogeneous_events() {
        use crate::multi_projector::MultiProjector;

        #[derive(Default, Clone)]
        struct AuditAll {
            seen: Arc<parking_lot::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl MultiProjector for AuditAll {
            const NAME: &'static str = "audit";
            const KINDS: &'static [&'static str] = &["a", "b"];

            async fn project(
                &self,
                event: &crate::types::RecordedEvent,
                _ctx: Ctx<'_>,
            ) -> Result<()> {
                self.seen.lock().push(event.event_type.clone());
                Ok(())
            }
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct A { a_id: Uuid, occurred_at: DateTime<Utc> }
        impl Event for A {
            const NAME: &'static str = "a";
            fn subject_id(&self) -> Uuid { self.a_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct B { b_id: Uuid, occurred_at: DateTime<Utc> }
        impl Event for B {
            const NAME: &'static str = "b";
            fn subject_id(&self) -> Uuid { self.b_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        let store = store();
        let auditor = AuditAll::default();
        let seen = auditor.seen.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_multi_projector(auditor)
        .build().await.unwrap();

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
        assert_eq!(names, vec!["a", "b", "a"]);

        engine.shutdown().await.unwrap();
    }

    // ── 0.3.1 fixes — caller-supplied envelope fields ──

    #[tokio::test]
    async fn emit_with_workflow_id_stamps_envelope() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).build().await.unwrap();

        let cmd_correlation = Uuid::new_v4();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .workflow_id(cmd_correlation)
        .await.unwrap();

        let events = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].workflow_id, cmd_correlation,
                   "persisted workflow_id MUST match caller-supplied id");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_with_parent_and_metadata_stamps_envelope() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).build().await.unwrap();

        let parent = Uuid::new_v4();

        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .causation_id(parent)
        .metadata("_run_id", "run-abc")
        .metadata("_schema_v", 2)
        .await.unwrap();

        let events = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events[0].causation_id, Some(parent));
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
    async fn emit_batch_propagates_workflow_id_to_every_fact() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build().await.unwrap();

        let id = Uuid::new_v4();
        let cmd_correlation = Uuid::new_v4();

        engine.emit(vec![
            CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
            CounterFact::Inc { by: 2, occurred_at: Utc::now(), counter_id: id },
        ])
        .workflow_id(cmd_correlation)
        .await.unwrap();

        let events = EventLogBackend::read_stream(
            store.as_ref(), <CounterFact as Event>::NAME, id, None,
        ).await.unwrap();
        assert_eq!(events.len(), 2);
        for ev in &events {
            assert_eq!(ev.workflow_id, cmd_correlation,
                       "every fact in the batch carries the caller's workflow_id");
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projector(UserRoster::default())
        .build().await.unwrap();

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
        // Multi-Event aggregates: same A::NAME registered with two
        // distinct Apply<F> impls is legitimate (e.g. PipelineState
        // folding ScrapeEvent + LifecycleEvent). The collision check
        // distinguishes "same NAME + same Event CATEGORY" (panic)
        // from "same NAME + different Event CATEGORYs" (allowed).

        #[derive(Default, Debug, Clone, Serialize, Deserialize)]
        struct Multi { hits: u32 }
        impl crate::aggregate::Aggregate for Multi {
            const NAME: &'static str = "Multi";
        }
        impl crate::aggregate::Apply<Tick> for Multi {
            fn apply(&mut self, _t: &Tick) { self.hits += 1; }
        }
        // Second Event type for the same Aggregate.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct Pong { pong_id: Uuid, occurred_at: DateTime<Utc> }
        impl Event for Pong {
            const NAME: &'static str = "pong";
            fn subject_id(&self) -> Uuid { self.pong_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }
        impl crate::aggregate::Apply<Pong> for Multi {
            fn apply(&mut self, _p: &Pong) { self.hits += 1; }
        }

        let store = store();
        let _engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![
            crate::aggregator::Aggregator::for_type::<Multi, Tick>(),
            crate::aggregator::Aggregator::for_type::<Multi, Pong>(),
        ])
        .build().await.unwrap();
        // No panic — different Event CATEGORYs.
    }

    #[tokio::test]
    #[should_panic(expected = "duplicate Aggregate::NAME `TickCounter`")]
    async fn registering_two_aggregators_with_same_name_panics() {
        // Two aggregators with the same Aggregate::NAME would collide
        // on the registry key `{NAME}:{id}` and silently overwrite
        // each other's state. EngineBuilder catches this at
        // registration time; duplicate names need distinct `A::NAME`
        // consts.
        let store = store();
        let _engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![
            crate::aggregator::Aggregator::for_type::<TickCounter, Tick>(),
            crate::aggregator::Aggregator::for_type::<TickCounter, Tick>(),
            // ↑ same NAME twice — panic here.
        ]);
    }

    #[tokio::test]
    #[should_panic(expected = "duplicate NAME `users`")]
    async fn registering_two_consumers_with_same_consumer_panics() {
        // Two UserRoster instances would share a cursor key — silent
        // corruption. EngineBuilder catches this at registration time.
        let store = store();
        let _engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
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
            type Event = UserCreated;
            const NAME: &'static str = "bulk.a";
            async fn project(&self, _f: &UserCreated, _: Ctx<'_>) -> Result<()> {
                self.hit.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        #[derive(Default)]
        struct B { hit: Arc<AtomicUsize> }
        #[async_trait]
        impl Projector for B {
            type Event = UserCreated;
            const NAME: &'static str = "bulk.b";
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projectors(vec![
            Box::new(a) as Box<dyn ProjectorRegistration>,
            Box::new(b) as Box<dyn ProjectorRegistration>,
        ])
        .build().await.unwrap();

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
        // Inspection path: emit Ticks for one subject_id, then read
        // TickCounter via engine.state_of::<A>(id).await.unwrap() without going
        // through a consumer.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![tick_aggregator()])
        .build().await.unwrap();

        // Tick.subject_id() returns Uuid::nil() — all ticks fold into
        // the nil-keyed slot.
        for i in 0..5 {
            engine.emit(Tick { seq: i, occurred_at: Utc::now() }).await.unwrap();
        }

        let counter = engine.state_of::<TickCounter>(Uuid::nil()).await.unwrap()
            .expect("snapshot Some after 5 ticks folded");
        assert_eq!(counter.count, 5,
                   "engine-level registry folded all 5 emitted Ticks");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "no aggregators were registered")]
    async fn engine_snapshot_panics_without_aggregators() {
        // No aggregators registered → snapshot panics loudly. The old
        // behavior (silent None) read as "no state yet" forever and hid
        // the misconfiguration — a real consumer shipped dedup gates
        // that never fired because of it.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).build().await.unwrap();
        let _ = engine.state_of::<TickCounter>(Uuid::nil()).await.unwrap();
    }

    #[tokio::test]
    async fn engine_on_terminal_failure_mapper_drains_synthesized_fact_to_log() {
        // End-to-end: register a reactor that always fails on
        // UserCreated. Configure on_terminal_failure to synthesize HandlerFailed.
        // After settle, the log contains the synthesized fact.

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct HandlerFailed {
            consumer: String,
            attempts: u32,
        }
        impl Event for HandlerFailed {
            const NAME: &'static str = "handler_failed";
            fn subject_id(&self) -> Uuid { Uuid::nil() }
        }

        struct AlwaysFails;
        #[async_trait]
        impl Reactor for AlwaysFails {
            type Trigger = UserCreated;
            const NAME: &'static str = "always-fails-e2e";
            async fn react(&self, _t: &UserCreated, _: Ctx<'_>) -> Result<Events> {
                Err(anyhow!("boom"))
            }
        }

        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .on_terminal_failure(|info: TerminalFailure| Some(HandlerFailed {
            consumer: info.consumer,
            attempts: info.attempts,
        }))
        .with_max_attempts(2)
        .with_reactor(AlwaysFails)
        .build().await.unwrap();

        let result = engine.emit(UserCreated {
            user_id: Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();

        // Wait until the reactor has retried + terminal-failure-mapped + the
        // synthetic fact has been appended to the log.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let events = EventLogBackend::read_all(
                store.as_ref(), LogCursor::ZERO, 10,
            ).await.unwrap();
            let terminal_failure_emitted = events.iter()
                .any(|e| e.event_type == "handler_failed");
            if terminal_failure_emitted { break; }
            assert!(std::time::Instant::now() < deadline,
                    "terminal-failure-mapped fact never made it to the log");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Trigger UserCreated + synthesized HandlerFailed both present.
        let events = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert!(events.iter().any(|e| e.event_type == "user_created"));
        assert!(events.iter().any(|e| e.event_type == "handler_failed"));

        let _ = result;
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn engine_settle_waits_for_reactor_chains_to_quiesce() {
        // The bug: settle waited only for direct consumers to reach
        // the emit position. Reactor outputs appended to the log and
        // their downstream consumers weren't covered.
        //
        // Setup:
        //   emit UserCreated
        //   → WelcomeReactor reacts, appends WelcomeQueued to the log
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
        impl Event for WelcomeQueuedFact {
            const NAME: &'static str = "welcome_queued";
            fn subject_id(&self) -> Uuid { self.user_id }
        }
        #[async_trait]
        impl Projector for WelcomeCounter {
            type Event = WelcomeQueuedFact;
            const NAME: &'static str = "settle.chain.counter";
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_reactor(WelcomeReactor)
        .with_projector(WelcomeCounter(counter_c))
        .build().await.unwrap();

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
                    WelcomeCounter chain quiesces");

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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projector(roster)
        .build().await.unwrap();

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
    async fn settle_scopes_to_one_run_while_another_floods_the_log() {
        // The point of scoped settle: run A's settle returns once A's chain
        // (UserCreated → WelcomeQueued) drains, EVEN WHILE run B keeps
        // appending to the shared log. A global-head settle would loop forever
        // here because the head never stops moving.
        let store = store();
        let engine = Arc::new(
            EngineBuilder::new(
                store.clone() as Arc<dyn EventLogBackend>,
                store.clone() as Arc<dyn CheckpointStore>,
                store.clone() as Arc<dyn ReactorCheckpoint>,
            )
            .with_reactor(WelcomeReactor)
            .build()
            .await
            .unwrap(),
        );

        // Run B: a background flood that never stops moving the global head.
        let flood = {
            let engine = engine.clone();
            let corr_b = Uuid::new_v4();
            tokio::spawn(async move {
                loop {
                    let _ = engine
                        .emit(UserCreated { user_id: Uuid::new_v4(), occurred_at: Utc::now() })
                        .workflow_id(corr_b)
                        .await;
                    tokio::time::sleep(Duration::from_millis(3)).await;
                }
            })
        };

        // Run A: emit once on its own workflow, then settle. With the old
        // global-head settle this would hang against run B's flood.
        let corr_a = Uuid::new_v4();
        let result = engine
            .emit(UserCreated { user_id: Uuid::new_v4(), occurred_at: Utc::now() })
            .workflow_id(corr_a)
            .await
            .unwrap();

        let settled =
            tokio::time::timeout(Duration::from_secs(5), engine.settle(result)).await;
        flood.abort();

        assert!(
            settled.is_ok(),
            "scoped settle must return for run A despite run B flooding the log"
        );
        settled.unwrap().unwrap();
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projector(roster)
        .build().await.unwrap();

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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projector(roster)
        .build().await.unwrap();

        // Without .settled(), bare .await would return before the
        // async projector ran. With .settled(), we are guaranteed
        // the projector has processed the fact.
        let result = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).settled().await.unwrap();

        assert_eq!(seen.lock().len(), 1,
                   ".settled() waited for the projector to observe");
        assert_ne!(result.workflow_id, Uuid::nil(),
                   "settled still surfaces the EmitResult");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn observer_captures_reactor_execution_and_aggregate_fold() {
        // End-to-end smoke test for the ReactorObserver pipeline.
        // Registers MemoryStore as the observer; emits a fact that
        // both folds into an aggregator AND triggers a reactor;
        // asserts every observability table was populated.
        use crate::reactor::{Events, Reactor};
        use std::sync::atomic::AtomicUsize;

        struct EchoReactor {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Reactor for EchoReactor {
            type Trigger = UserCreated;
            const NAME: &'static str = "observer-echo";
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_observer(store.clone())
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .with_reactor(EchoReactor { calls: calls.clone() })
        .build().await.unwrap();

        let user_id = Uuid::new_v4();
        engine.emit(UserCreated {
            user_id,
            occurred_at: Utc::now(),
        }).settled().await.unwrap();

        // Reactor ran.
        assert!(calls.load(Ordering::SeqCst) >= 1, "reactor fired");

        // Observer hook #1: reactor_started + reactor_completed populated
        // reactor_executions for (event_id, NAME).
        let execs = store.reactor_executions();
        assert!(
            execs.iter().any(|e| {
                let (_eid, rid) = e.key();
                rid == "observer-echo"
            }),
            "reactor_executions populated with the reactor's NAME"
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
        // emitted for a given subject_id, the aggregate hasn't been
        // materialized — return None rather than a fresh default
        // (which would silently fool callers asserting on existence).
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<UserAgg, UserCreated>()])
        .build().await.unwrap();

        let id = Uuid::new_v4();
        assert!(engine.state_of::<UserAgg>(id).await.unwrap().is_none(),
                "snapshot returns None when the aggregate has no facts yet");
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_returns_some_clone_after_emit() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([Aggregator::for_type::<Counter, CounterFact>()])
        .build().await.unwrap();

        let id = Uuid::new_v4();
        engine.emit(CounterFact::Inc {
            by: 7, occurred_at: Utc::now(), counter_id: id,
        }).await.unwrap();

        let state = engine.state_of::<Counter>(id).await.unwrap()
            .expect("snapshot Some after emit folded a fact for this id");
        assert_eq!(state.value, 7);

        // Independent ids stay None.
        let other = Uuid::new_v4();
        assert!(engine.state_of::<Counter>(other).await.unwrap().is_none(),
                "snapshot is keyed — other ids unaffected");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_result_carries_workflow_id() {
        // EmitResult.workflow_id must surface the chain id that was
        // stamped on the emitted fact(s). Clients use it to poll
        // workflow projections; tests use it to scope `settle_to`
        // and `snapshot::<A>(subject_id)` to one run.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).build().await.unwrap();

        // Auto-generated workflow when not provided.
        let r1 = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();
        assert_ne!(r1.workflow_id, Uuid::nil(),
                   "emit auto-generates workflow_id when not set");

        // Two emits without explicit workflow must produce
        // distinct workflows (each is a fresh chain).
        let r2 = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await.unwrap();
        assert_ne!(r1.workflow_id, r2.workflow_id,
                   "distinct emits get distinct workflow_ids");

        // Explicit workflow: result echoes what the caller set.
        let wf = Uuid::new_v4();
        let r3 = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .workflow_id(wf)
        .await.unwrap();
        assert_eq!(r3.workflow_id, wf,
                   "EmitResult.workflow_id echoes the explicit value");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_with_empty_batch_is_a_no_op() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).build().await.unwrap();

        let result = engine.emit(Vec::<UserCreated>::new()).await.unwrap();
        assert_eq!(result.position, LogCursor::ZERO);
        // Empty emit still produces a fresh workflow_id (no facts
        // got stamped with it; the value is informational).
        assert_ne!(result.workflow_id, Uuid::nil());

        // No events written.
        let events = EventLogBackend::read_all(
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_default_metadata(defaults)
        .build().await.unwrap();

        // Per-emit override: _run_id should win; _actor inherited; new
        // key _trace merges in.
        engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .metadata("_run_id", "run-override")
        .metadata("_trace", "abc123")
        .await.unwrap();

        let events = EventLogBackend::read_all(
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projector(UserRoster::default())
        .build().await.unwrap();

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
    impl Event for Tick {
        const NAME: &'static str = "tick";
        fn subject_id(&self) -> Uuid { Uuid::nil() }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    struct TickCounter { count: u32 }
    impl crate::aggregate::Aggregate for TickCounter {
        const NAME: &'static str = "TickCounter";
    }
    impl crate::aggregate::Apply<Tick> for TickCounter {
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
            type Event = Tick;
            const NAME: &'static str = "ticks";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                let s = ctx.state_of::<TickCounter>(Uuid::nil()).await?.curr;
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![tick_aggregator()])
        .with_projector(cap)
        .build().await.unwrap();

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
            const NAME: &'static str = "ticker.reactor";
            async fn react(
                &self, _t: &Tick, ctx: Ctx<'_>,
            ) -> Result<crate::reactor::Events> {
                let s = ctx.state_of::<TickCounter>(Uuid::nil()).await?;
                self.transitions.lock().push((s.prev.count, s.curr.count));
                Ok(crate::reactor::Events::new())
            }
        }

        let store = store();
        let cap = Capture { transitions: Arc::new(parking_lot::Mutex::new(Vec::new())) };
        let transitions = cap.transitions.clone();

        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![tick_aggregator()])
        .with_reactor(cap)
        .build().await.unwrap();

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
        let result = crate::append_event(store, EventData {
            event_id:        Uuid::new_v4(),
            causation_id:       None,
            workflow_id:  Uuid::new_v4(),
            event_type:      "tick".into(),
            payload:         serde_json::to_value(&tick).unwrap(),
            created_at:      tick.occurred_at,
            category:  None,
            subject_id:    None,
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
    async fn project_failure_keeps_fold_and_retry_does_not_double_count() {
        // A2 contract: fold tracks the log, not body success. Projector
        // succeeds on event 1, errors on event 2 — after the failed
        // step the registry holds count=2 (event 2 IS in the log; its
        // fold is not rolled back). The retried step re-delivers event
        // 2; the idempotency gate skips the re-fold, so count stays 2
        // and the body completes. (The old capture/restore rollback
        // this replaces could permanently desync registry from cursor
        // on the terminal-failure path.)
        struct FailsOnSecond { calls: Arc<AtomicUsize> }
        #[async_trait]
        impl Projector for FailsOnSecond {
            type Event = Tick;
            const NAME: &'static str = "rollback.test";
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
        assert_eq!(curr.count, 2,
                   "fold persists through body failure — state reflects the log");

        // Supervisor retry: event 2 re-delivers, its fold skips, the
        // body succeeds this time. No double-count.
        let result = runner.step(10).await;
        assert!(result.is_ok(), "retried step succeeds");
        let (_, curr) = aggs.get_singleton_arc::<TickCounter>();
        assert_eq!(curr.count, 2,
                   "re-delivered fold is an idempotent skip — never double-counted");
    }

    /// F9.6 — pin the apply→project interleaving contract for batch
    /// processing.
    ///
    /// When `ProjectionRunner::step` drains N events in one batch, the
    /// runtime must fold each event into the aggregator registry
    /// *and then* invoke `project()` for that event, before moving on
    /// to event N+1. The projector sees `(prev, curr)` corresponding
    /// to its own event — not the post-batch state for every call.
    ///
    /// This matters because the "obvious" alternative — apply ALL
    /// facts, then project ALL — would silently break transition
    /// guards that depend on per-event `prev`/`curr` deltas
    /// (`ctx.state_of::<A>(Uuid::nil()).prev` vs `.curr`). Today's projection
    /// runner does the right thing; this test makes the contract
    /// load-bearing so a future refactor can't silently flip to
    /// apply-all-then-project-all.
    ///
    /// Setup: append 5 ticks to the log, then run `step(10)` from a
    /// fresh runner (cursor=ZERO). Expect per-event transitions
    /// `[(0,1), (1,2), (2,3), (3,4), (4,5)]`. If apply-all-first
    /// regressed in, every projector call would see `(4, 5)`.
    #[tokio::test]
    async fn projector_batch_sees_per_event_prev_curr_interleaved() {
        #[derive(Clone)]
        struct Capture {
            transitions: Arc<parking_lot::Mutex<Vec<(u32, u32)>>>,
        }
        #[async_trait]
        impl Projector for Capture {
            type Event = Tick;
            const NAME: &'static str = "batch.interleave";
            async fn project(
                &self,
                _f: &Tick,
                ctx: Ctx<'_>,
            ) -> Result<()> {
                let s = ctx.state_of::<TickCounter>(Uuid::nil()).await?;
                self.transitions.lock().push((s.prev.count, s.curr.count));
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        for i in 0..5 {
            append_tick(&store, i).await;
        }

        let cap = Capture {
            transitions: Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let transitions = cap.transitions.clone();

        let runner = ProjectionRunner::new(
            cap,
            "batch.interleave",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        )
        .with_aggregators(fresh_tick_registry());

        runner.step(10).await.unwrap();

        assert_eq!(
            *transitions.lock(),
            vec![(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)],
            "single batch must interleave apply→project per event so each \
             projector call sees its own (prev, curr) — not a post-batch \
             fixed view"
        );
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
            type Event = Tick;
            const NAME: &'static str = "hydration.bug";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                let n = ctx.state_of::<TickCounter>(Uuid::nil()).await?.curr.count;
                self.snaps.lock().push(n);
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
            type Event = Tick;
            const NAME: &'static str = "hydrate.cold";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                *self.snap.lock() = Some(ctx.state_of::<TickCounter>(Uuid::nil()).await?.curr.count);
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

    /// Transition-guard behavior after cold-start replay.
    ///
    /// Pinned per the P11.a-audit follow-up (`docs/audits/2026-05-14-
    /// p11a-legacy-test-parity-audit.md`, HIGH-severity gap
    /// `transition_guard_correct_after_log_replay`).
    ///
    /// Setup: 2 historical Tick events in the log, checkpoint past
    /// them; one fresh Tick arrives. A consumer that reads
    /// `ctx.state_of::<TickCounter>(Uuid::nil()).prev.count` vs `.curr.count`
    /// must see `(2, 3)` — the historical state from hydration vs
    /// the post-fold state for the new event. If hydration only
    /// folds into `curr` and leaves `prev` at default, transition
    /// guards (`prev != curr`) fire spuriously on the first event
    /// after restart, breaking gates rootsignal depends on.
    #[tokio::test]
    async fn transition_guard_prev_curr_correct_after_cold_start_hydration() {
        let store = Arc::new(MemoryStore::new());
        append_tick(&store, 0).await;
        let pos2 = append_tick(&store, 1).await;
        // Checkpoint past the historical events — simulates a
        // restart that resumes from a known cursor.
        store.set("hydrate.transition", pos2).await.unwrap();
        // New event the runner picks up after hydration.
        append_tick(&store, 2).await;

        #[derive(Clone)]
        struct Capture {
            prev_curr: Arc<parking_lot::Mutex<Option<(u32, u32)>>>,
        }
        #[async_trait]
        impl Projector for Capture {
            type Event = Tick;
            const NAME: &'static str = "hydrate.transition";
            async fn project(
                &self,
                _f: &Tick,
                ctx: Ctx<'_>,
            ) -> Result<()> {
                let s = ctx.state_of::<TickCounter>(Uuid::nil()).await?;
                *self.prev_curr.lock() = Some((s.prev.count, s.curr.count));
                Ok(())
            }
        }

        let cap = Capture {
            prev_curr: Arc::new(parking_lot::Mutex::new(None)),
        };
        let prev_curr = cap.prev_curr.clone();

        let runner = ProjectionRunner::new(
            cap,
            "hydrate.transition",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        )
        .with_aggregators(fresh_tick_registry());

        runner.step(10).await.unwrap();

        assert_eq!(
            *prev_curr.lock(),
            Some((2, 3)),
            "after replaying 2 historical events, the next event's projector \
             body must see prev=2 (historical fold state) and curr=3 \
             (post-fold state) — the contract that lets transition guards \
             survive cold-start"
        );
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
            type Event = Tick;
            const NAME: &'static str = "reader";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                let _ = ctx.state_of::<TickCounter>(Uuid::nil()).await?.curr;
                Ok(())
            }
        }

        let store = store();
        // Append one Tick directly to the log.
        let tick = Tick { seq: 0, occurred_at: Utc::now() };
        let payload = serde_json::to_value(&tick).unwrap();
        crate::append_event(store.as_ref(), EventData {
            event_id:        Uuid::new_v4(),
            causation_id:       None,
            workflow_id:  Uuid::new_v4(),
            event_type:      "tick".into(),
            payload,
            created_at:      tick.occurred_at,
            category:  None,
            subject_id:    None,
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
            type Event = Tick;
            const NAME: &'static str = "panic.recovery";
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_projector(m)
        .build().await.unwrap();

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
        impl crate::aggregate::Aggregate for OtherCounter {
            const NAME: &'static str = "OtherCounter";
        }
        impl crate::aggregate::Apply<Tick> for OtherCounter {
            fn apply(&mut self, _: &Tick) { self.count += 1; }
        }

        #[derive(Clone)]
        struct VerifyBoth {
            a: Arc<parking_lot::Mutex<Vec<u32>>>,
            b: Arc<parking_lot::Mutex<Vec<u32>>>,
        }
        #[async_trait]
        impl Projector for VerifyBoth {
            type Event = Tick;
            const NAME: &'static str = "accum.test";
            async fn project(
                &self, _f: &Tick, ctx: Ctx<'_>,
            ) -> Result<()> {
                let na = ctx.state_of::<TickCounter>(Uuid::nil()).await?.curr.count;
                self.a.lock().push(na);
                let nb = ctx.state_of::<OtherCounter>(Uuid::nil()).await?.curr.count;
                self.b.lock().push(nb);
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![agg_a])     // first call
        .with_aggregators(vec![agg_b])     // second call — must accumulate
        .with_projector(v)
        .build().await.unwrap();

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
        impl Event for OtherFact {
            const NAME: &'static str = "happening";
            fn subject_id(&self) -> Uuid { self.id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        #[derive(Clone)]
        struct OnlyTickRouter {
            seen: Arc<parking_lot::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl MultiProjector for OnlyTickRouter {
            const NAME: &'static str = "tick.only";
            const KINDS: &'static [&'static str] = &["tick"];
            async fn project(
                &self,
                event: &crate::types::RecordedEvent,
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
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_multi_projector(router)
        .build().await.unwrap();

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
        assert!(s.iter().all(|t| t == "tick"),
                "every delivered event matches the declared subscription");

        engine.shutdown().await.unwrap();
    }
}
