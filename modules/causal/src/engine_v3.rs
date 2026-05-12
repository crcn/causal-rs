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

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::aggregate_v3::Aggregate;
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
// Aggregate-stream gate (partial BS1 closure)
// ─────────────────────────────────────────────────────────────────────
//
// `with_aggregate::<A>()` records `A::CATEGORY` as an OCC-required
// stream. `Engine::emit` rejects writes to those streams with
// `EmitError::OccStreamMisuse`. Streams not in the set are accepted
// (default permissive — preserves Phase 4d behavior).
//
// AUDIT NOTE: this only PARTIALLY closes BS1. Truly closing it would
// require flipping the default to "reject unregistered" + forcing
// every emit-able category to be explicitly registered. That's a
// behavior break the audit deferred. Tracked in the design doc's
// black-swan table as "BS1: regressed; future phase to flip default."

/// Errors returned by `Engine::emit` when stream-policy enforcement
/// kicks in.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("category `{category}` is registered as OccRequired \
             (aggregate stream); use Engine::append::<A> with an \
             expected_version instead of emit")]
    OccStreamMisuse { category: String },
}

/// Caller-supplied envelope fields for `emit_in` / `append_in`.
///
/// `Engine::emit` and `Engine::append` (without `_in`) auto-generate
/// `correlation_id` and leave `parent_id` + `metadata` empty — fine
/// for ingestion gateways that originate events. Command handlers
/// responding to upstream requests SHOULD use `_in` and propagate
/// `correlation_id` + `parent_id` from the trigger so causal-chain
/// tracing works across the system.
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// `None` → engine generates a fresh `Uuid::new_v4()`.
    pub correlation_id: Option<Uuid>,
    /// `None` → no parent (root event).
    pub parent_id:      Option<Uuid>,
    /// Empty by default. Stamp `_run_id`, `_schema_v`, `_phase` etc. here.
    pub metadata:       Metadata,
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

pub struct EngineBuilder {
    log:                   Arc<dyn EventLogBackend>,
    checkpoint:            Arc<dyn CheckpointStore>,
    outbox:                Arc<dyn ReactorOutbox>,
    consumers:             Vec<RunnerFactory>,
    occ_required_streams:  std::collections::HashSet<String>,
    aggregators:           Vec<Aggregator>,
    group_names:           std::collections::HashSet<String>,
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
            occ_required_streams: std::collections::HashSet::new(),
            aggregators: Vec::new(),
            group_names: std::collections::HashSet::new(),
        }
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

    /// Register an aggregate. Marks `A::CATEGORY` as OCC-required so
    /// `Engine::emit` rejects writes to that stream — only
    /// `Engine::append<A>(expected, ...)` writes (per C11). The
    /// `Engine::load<A>` and `Engine::append<A>` methods wire the
    /// command-handler path.
    pub fn with_aggregate<A: Aggregate>(mut self) -> Self {
        self.occ_required_streams.insert(A::CATEGORY.into());
        self
    }

    /// Register [`crate::Aggregator`] definitions so consumers can read
    /// folded singleton/keyed aggregate state via `ctx.aggregate::<A>()`
    /// and `ctx.aggregate_of::<A>(id)`.
    ///
    /// Aggregator state is **per-engine, in-memory**. It does NOT
    /// persist across `Engine` instances. For saga-pattern read-only
    /// state shared across reactors *within* one workflow run, this
    /// is the right tool. For long-lived aggregates spanning
    /// processes, see `docs/aggregate-state-scope.md` for what's
    /// missing and when to add it.
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
        self.aggregators.extend(aggregators);
        self
    }

    pub fn with_projector<P: Projector + 'static>(mut self, p: P) -> Self
    where
        P::Fact: DeserializeOwned,
    {
        self.claim_group_name(P::GROUP_NAME);
        let log = self.log.clone();
        let checkpoint = self.checkpoint.clone();
        self.consumers.push(Box::new(move |aggs| {
            let mut runner = ProjectionRunner::new(p, P::GROUP_NAME, log, checkpoint);
            if let Some(aggs) = aggs { runner = runner.with_aggregators(aggs); }
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
        self.consumers.push(Box::new(move |aggs| {
            let mut runner = ReactorRunner::new(r, R::GROUP_NAME, log, outbox);
            if let Some(aggs) = aggs { runner = runner.with_aggregators(aggs); }
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
        self.consumers.push(Box::new(move |aggs| {
            let mut runner = MultiProjectorRunner::new(p, P::GROUP_NAME, log, checkpoint);
            if let Some(aggs) = aggs { runner = runner.with_aggregators(aggs); }
            Arc::new(runner) as Arc<dyn Supervisable>
        }));
        self
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
        let consumers: Vec<Arc<dyn Supervisable>> = self.consumers
            .into_iter()
            .map(|f| f(make_registry()))
            .collect();
        Engine::start(
            self.log,
            self.checkpoint,
            self.outbox,
            consumers,
            self.occ_required_streams,
        )
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
    occ_required_streams:  std::collections::HashSet<String>,
}

impl Engine {
    fn start(
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
        outbox: Arc<dyn ReactorOutbox>,
        consumers: Vec<Arc<dyn Supervisable>>,
        occ_required_streams: std::collections::HashSet<String>,
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

        // Relay supervisor: drain reactor outbox into the log.
        let relay = RelayLoop::new(log.clone(), outbox.clone());
        let mut relay_rx = shutdown_tx.subscribe();
        let relay_task = tokio::spawn(async move {
            supervise_relay(relay, &mut relay_rx).await;
        });
        handles.push(relay_task);

        Self { log, checkpoint, shutdown_tx, handles, occ_required_streams }
    }

    /// Emit a fact to the log with auto-generated correlation_id and
    /// no parent / metadata. Convenience for ingestion gateways or
    /// tests. Command handlers responding to upstream requests should
    /// use [`emit_in`](Self::emit_in) to propagate correlation_id /
    /// parent_id from the trigger.
    pub async fn emit<F: Fact>(&self, fact: F) -> Result<LogCursor> {
        self.emit_in(fact, WriteOptions::default()).await
    }

    /// Emit a fact with explicit envelope fields. Rejects with
    /// `EmitError::OccStreamMisuse` if the fact's stream category
    /// was registered as OCC-required (via
    /// `EngineBuilder::with_aggregate`). Partially closes BS1 — see
    /// the BS table in the design doc.
    pub async fn emit_in<F: Fact>(
        &self,
        fact: F,
        opts: WriteOptions,
    ) -> Result<LogCursor> {
        let category = F::CATEGORY;
        if self.occ_required_streams.contains(category) {
            return Err(anyhow!(EmitError::OccStreamMisuse {
                category: category.into(),
            }));
        }
        let event_type = format!("{}:{}", category, fact.name());
        // Producer-claimed time wins; otherwise persistence stamps now.
        let occurred_at = fact.occurred_at().unwrap_or_else(Utc::now);
        let stream_id = fact.stream_id();
        let payload = serde_json::to_value(&fact)?;
        let new_event = NewEvent {
            event_id:        Uuid::new_v4(),
            parent_id:       opts.parent_id,
            correlation_id:  opts.correlation_id.unwrap_or_else(Uuid::new_v4),
            event_type,
            payload,
            created_at:      occurred_at,
            aggregate_type:  Some(category.to_string()),
            aggregate_id:    Some(stream_id),
            metadata:        opts.metadata,
            ephemeral:       None,
            persistent:      true,
        };
        let result = self.log.append(new_event).await?;
        Ok(result.position)
    }

    /// Hydrate an aggregate by folding its stream from version 0.
    /// Returns the aggregate state and the current stream version
    /// (for use as `expected` in a subsequent `append`).
    ///
    /// Aggregate streams use `aggregate_type = A::CATEGORY` and
    /// `aggregate_id = id`; the same convention `Engine::append<A>`
    /// uses on write.
    pub async fn load<A: Aggregate>(
        &self,
        id: Uuid,
    ) -> Result<(A, StreamVersion)>
    where
        A::Fact: DeserializeOwned,
    {
        let events = self.log.load_stream(A::CATEGORY, id, None).await?;
        let mut agg = A::default();
        let mut version = StreamVersion::ZERO;
        for event in events {
            let fact: A::Fact = serde_json::from_value(event.payload)?;
            agg.apply(&fact);
            if let Some(v) = event.version {
                version = v;
            }
        }
        Ok((agg, version))
    }

    /// CAS-protected append to an aggregate stream (per C6).
    /// Convenience over [`append_in`](Self::append_in) with default
    /// envelope (auto correlation_id, no parent, empty metadata).
    pub async fn append<A: Aggregate>(
        &self,
        id: Uuid,
        expected: StreamVersion,
        facts: Vec<A::Fact>,
    ) -> Result<StreamVersion> {
        self.append_in::<A>(id, expected, facts, WriteOptions::default()).await
    }

    /// CAS-protected append with explicit envelope fields. Each fact
    /// in the batch carries the same `correlation_id` and `parent_id`
    /// from `opts`. Errors with `ConflictError` if the stream has
    /// moved past `expected`. The standard recovery is to `load<A>`
    /// again, re-decide on the new state, and retry. On failure
    /// mid-batch, prior appends remain durable.
    pub async fn append_in<A: Aggregate>(
        &self,
        id: Uuid,
        expected: StreamVersion,
        facts: Vec<A::Fact>,
        opts: WriteOptions,
    ) -> Result<StreamVersion> {
        let correlation = opts.correlation_id.unwrap_or_else(Uuid::new_v4);
        let mut current_expected = expected;
        let mut last_version = expected;
        for fact in facts {
            let event_type = format!("{}:{}", <A::Fact as Fact>::CATEGORY, fact.name());
            let occurred_at = fact.occurred_at().unwrap_or_else(Utc::now);
            let payload = serde_json::to_value(&fact)?;
            let new_event = NewEvent {
                event_id:        Uuid::new_v4(),
                parent_id:       opts.parent_id,
                correlation_id:  correlation,
                event_type,
                payload,
                created_at:      occurred_at,
                aggregate_type:  Some(A::CATEGORY.to_string()),
                aggregate_id:    Some(id),
                metadata:        opts.metadata.clone(),
                ephemeral:       None,
                persistent:      true,
            };
            let result = self.log
                .append_to_stream(A::CATEGORY, id, current_expected, new_event)
                .await?;
            last_version = result.version.unwrap_or(current_expected);
            current_expected = last_version;
        }
        Ok(last_version)
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
    use crate::reactor::Events;
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
    impl crate::event::Event for WelcomeQueued {
        fn durable_name(&self) -> &str { "welcome:welcome_queued" }
        fn event_prefix() -> &'static str { "welcome" }
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

    // ── Phase 5a — Aggregate-stream emit gating ──

    /// Aggregate registered for stream-policy testing.
    #[derive(Default)]
    struct UserAgg;
    impl crate::aggregate_v3::Aggregate for UserAgg {
        type Fact = UserCreated;
        const CATEGORY: &'static str = "user";
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
    async fn emit_to_aggregate_stream_returns_occ_stream_misuse() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregate::<UserAgg>()
        .build();

        let result = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await;

        assert!(result.is_err(), "emit to aggregate stream MUST error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("OccRequired") || err_msg.contains("user"),
                "error message identifies the misused category: {}", err_msg);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn aggregate_registration_does_not_affect_other_streams() {
        // Register UserAgg on category "user". A different fact whose
        // stream lives in a different category should still be emit-able.
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregate::<UserAgg>()
        .build();

        // OrderPlaced lives in category "order" — unregistered, default
        // OpenAppend.
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

        // But user-category emits still rejected.
        let user_result = engine.emit(UserCreated {
            user_id:     Uuid::new_v4(),
            occurred_at: Utc::now(),
        }).await;
        assert!(user_result.is_err());

        engine.shutdown().await.unwrap();
    }

    // ── Phase 5b — load + append with OCC ──

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

    #[derive(Default, Debug, PartialEq)]
    struct Counter { value: i32 }
    impl crate::aggregate_v3::Aggregate for Counter {
        type Fact = CounterFact;
        const CATEGORY: &'static str = "counter";
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
        .with_aggregate::<Counter>()
        .build();

        let id = Uuid::new_v4();
        let (agg, ver) = engine.load::<Counter>(id).await.unwrap();
        assert_eq!(agg, Counter::default());
        assert_eq!(ver, StreamVersion::ZERO);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn append_then_load_round_trips_aggregate_state() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregate::<Counter>()
        .build();

        let id = Uuid::new_v4();
        let v1 = engine.append::<Counter>(id, StreamVersion::ZERO, vec![
            CounterFact::Inc { by: 3, occurred_at: Utc::now(), counter_id: id },
            CounterFact::Inc { by: 5, occurred_at: Utc::now(), counter_id: id },
        ]).await.unwrap();
        assert_eq!(v1, StreamVersion::from_raw(2),
                   "version is 2 after appending 2 facts");

        let (agg, ver) = engine.load::<Counter>(id).await.unwrap();
        assert_eq!(agg.value, 8);
        assert_eq!(ver, StreamVersion::from_raw(2));

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn append_with_stale_expected_version_returns_conflict() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregate::<Counter>()
        .build();

        let id = Uuid::new_v4();

        // First append moves version to 1.
        engine.append::<Counter>(id, StreamVersion::ZERO, vec![
            CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
        ]).await.unwrap();

        // Stale expected (still ZERO) → ConflictError.
        let result = engine.append::<Counter>(id, StreamVersion::ZERO, vec![
            CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
        ]).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let conflict = err.downcast_ref::<crate::event_log::ConflictError>()
            .expect("expected ConflictError");
        assert_eq!(conflict.expected, StreamVersion::ZERO);
        assert_eq!(conflict.current, StreamVersion::from_raw(1));

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_appends_to_same_aggregate_one_wins_one_conflicts() {
        // C6 in action under concurrent load. Two callers both load
        // the aggregate at version 0, both decide, both call
        // append::<A>(0, ...). The atomic single-mutex MemoryStore
        // override of append_to_stream serializes them: one wins
        // (version 1), the other gets ConflictError.
        let store = store();
        let engine = Arc::new(
            EngineBuilder::new(
                store.clone() as Arc<dyn EventLogBackend>,
                store.clone() as Arc<dyn CheckpointStore>,
                store.clone() as Arc<dyn ReactorOutbox>,
            )
            .with_aggregate::<Counter>()
            .build()
        );

        let id = Uuid::new_v4();

        let e1 = engine.clone();
        let e2 = engine.clone();
        let h1 = tokio::spawn(async move {
            e1.append::<Counter>(id, StreamVersion::ZERO, vec![
                CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
            ]).await
        });
        let h2 = tokio::spawn(async move {
            e2.append::<Counter>(id, StreamVersion::ZERO, vec![
                CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
            ]).await
        });
        let (r1, r2) = tokio::join!(h1, h2);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        let winners = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        let losers  = [&r1, &r2].iter().filter(|r| r.is_err()).count();
        assert_eq!(winners, 1, "exactly one winner");
        assert_eq!(losers, 1, "exactly one conflict");

        // Final aggregate state: one Inc applied → value == 1.
        let (agg, ver) = engine.load::<Counter>(id).await.unwrap();
        assert_eq!(agg.value, 1);
        assert_eq!(ver, StreamVersion::from_raw(1));
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
        .with_aggregate::<Counter>()
        .build();

        let id = Uuid::new_v4();
        let pinned = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap().with_timezone(&Utc);
        engine.append::<Counter>(id, StreamVersion::ZERO, vec![
            CounterFact::Inc { by: 1, occurred_at: pinned, counter_id: id },
        ]).await.unwrap();

        let events = EventLogBackend::load_stream(
            store.as_ref(), Counter::CATEGORY, id, None,
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
    async fn emit_in_uses_caller_supplied_correlation_id() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();

        let cmd_correlation = Uuid::new_v4();

        engine.emit_in(
            UserCreated {
                user_id:     Uuid::new_v4(),
                occurred_at: Utc::now(),
            },
            WriteOptions {
                correlation_id: Some(cmd_correlation),
                parent_id:      None,
                metadata:       Metadata::new(),
            },
        ).await.unwrap();

        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].correlation_id, cmd_correlation,
                   "persisted correlation_id MUST match caller-supplied id");

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn emit_in_uses_caller_supplied_parent_and_metadata() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).build();

        let parent = Uuid::new_v4();
        let mut meta = Metadata::new();
        meta.insert("_run_id".into(), serde_json::json!("run-abc"));
        meta.insert("_schema_v".into(), serde_json::json!(2));

        engine.emit_in(
            UserCreated {
                user_id:     Uuid::new_v4(),
                occurred_at: Utc::now(),
            },
            WriteOptions {
                correlation_id: None,
                parent_id:      Some(parent),
                metadata:       meta.clone(),
            },
        ).await.unwrap();

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
    async fn append_in_propagates_correlation_id_to_every_emitted_fact() {
        let store = store();
        let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorOutbox>,
        )
        .with_aggregate::<Counter>()
        .build();

        let id = Uuid::new_v4();
        let cmd_correlation = Uuid::new_v4();

        engine.append_in::<Counter>(
            id,
            StreamVersion::ZERO,
            vec![
                CounterFact::Inc { by: 1, occurred_at: Utc::now(), counter_id: id },
                CounterFact::Inc { by: 2, occurred_at: Utc::now(), counter_id: id },
            ],
            WriteOptions {
                correlation_id: Some(cmd_correlation),
                parent_id:      None,
                metadata:       Metadata::new(),
            },
        ).await.unwrap();

        let events = EventLogBackend::load_stream(
            store.as_ref(), Counter::CATEGORY, id, None,
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
        }).await.unwrap();

        // 0.3.1 signature: no checkpoint param.
        engine.await_observed_by("users", pos).await.unwrap();

        engine.shutdown().await.unwrap();
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
    impl crate::event::Event for Tick {
        fn durable_name(&self) -> &str { "ticker:tick" }
        fn event_prefix() -> &'static str { "ticker" }
    }

    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    struct TickCounter { count: u32 }
    impl crate::aggregator::Aggregate for TickCounter {
        fn aggregate_type() -> &'static str { "TickCounter" }
    }
    impl crate::aggregator::Apply<Tick> for TickCounter {
        fn apply(&mut self, _e: Tick) { self.count += 1; }
    }

    fn tick_aggregator() -> crate::aggregator::Aggregator {
        crate::aggregator::Aggregator::new::<Tick, TickCounter, _>(|_t| Uuid::nil())
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
            ) -> Result<crate::reactor::Events> {
                let s = ctx.aggregate::<TickCounter>();
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
        impl crate::aggregator::Aggregate for OtherCounter {
            fn aggregate_type() -> &'static str { "OtherCounter" }
        }
        impl crate::aggregator::Apply<Tick> for OtherCounter {
            fn apply(&mut self, _: Tick) { self.count += 1; }
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

        let agg_a = crate::aggregator::Aggregator::new::<Tick, TickCounter, _>(|_| Uuid::nil());
        let agg_b = crate::aggregator::Aggregator::new::<Tick, OtherCounter, _>(|_| Uuid::nil());

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
