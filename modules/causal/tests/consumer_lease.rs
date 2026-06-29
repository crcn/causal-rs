//! Consumer-lease integration tests.
//!
//! Tests use a `MockLeasor` and `MemoryStore` — no Postgres required.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::consumer_lease::{ConsumerLeasor, LeaseGuard};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::projection_runner::ProjectionRunner;
use causal::projector::Projector;
use causal::reactor::{Events, Reactor};
use causal::reactor_runner::ReactorRunner;
use causal::types::EventData;
use causal::EngineBuilder;

// ── Trigger fact ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    id: Uuid,
    occurred_at: DateTime<Utc>,
}
impl Event for Ping {
    const NAME: &'static str = "lease_ping";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

// ── Simple projector ──────────────────────────────────────────────────

#[derive(Clone)]
struct PingProjector(Arc<AtomicUsize>);

#[async_trait]
impl Projector for PingProjector {
    type Event = Ping;
    const NAME: &'static str = "lease-projector";
    async fn project(&self, _p: &Ping, _ctx: Ctx<'_>) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ── Simple reactor ────────────────────────────────────────────────────

struct Counter(Arc<AtomicUsize>);

#[async_trait]
impl Reactor for Counter {
    type Trigger = Ping;
    const NAME: &'static str = "lease-counter";
    async fn react(&self, _t: &Ping, _ctx: Ctx<'_>) -> Result<Events> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Events::new())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

fn append_ping(store: &MemoryStore) {
    let ev = EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: "lease_ping".to_string(),
        payload: serde_json::to_value(Ping {
            id: Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .unwrap(),
        created_at: Utc::now(),
        category: Some("lease_ping".to_string()),
        subject_id: Some(Uuid::new_v4()),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    let _ = futures::executor::block_on(causal::event_log::append_event(store, ev)).unwrap();
}

/// Step the runner until there are no more events and no pending triggers.
/// Panics if quiescence takes > 5 seconds.
async fn step_until_quiescent(runner: &ReactorRunner<Counter>) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        runner.step(256).await?;
        // The runner holds no DispatchState externally — check by reading
        // the log again. If step returns Idle twice in a row we're done.
        let outcome = runner.step(256).await?;
        if matches!(outcome, causal::projection_runner::StepOutcome::Idle) {
            // Give workers a moment to deliver any in-flight completions
            tokio::time::sleep(Duration::from_millis(5)).await;
            let final_outcome = runner.step(256).await?;
            if matches!(final_outcome, causal::projection_runner::StepOutcome::Idle) {
                return Ok(());
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "runner did not quiesce within 5s",
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn make_store() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::new())
}

// ── Mock leasor ───────────────────────────────────────────────────────

/// A simple leasor that records how many acquires have happened.
struct MockLeasor {
    acquired: Arc<AtomicUsize>,
}

struct MockGuard;
impl LeaseGuard for MockGuard {}

impl MockLeasor {
    fn new() -> (Arc<Self>, Arc<AtomicUsize>) {
        let acquired = Arc::new(AtomicUsize::new(0));
        (Arc::new(Self { acquired: acquired.clone() }), acquired)
    }
}

#[async_trait]
impl ConsumerLeasor for MockLeasor {
    async fn acquire(&self, _consumer_id: &str) -> Result<Box<dyn LeaseGuard>> {
        self.acquired.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockGuard))
    }
}

/// A leasor that immediately returns an error.
struct ErrorLeasor;

#[async_trait]
impl ConsumerLeasor for ErrorLeasor {
    async fn acquire(&self, _: &str) -> Result<Box<dyn LeaseGuard>> {
        Err(anyhow::anyhow!("lease acquisition failed: simulated error"))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

/// Baseline: a runner without a leasor processes triggers normally.
#[tokio::test]
async fn runner_without_leasor_processes_normally() -> Result<()> {
    let store = make_store();
    append_ping(&store);
    append_ping(&store);

    let counter = Arc::new(AtomicUsize::new(0));
    let runner = ReactorRunner::new(
        Counter(counter.clone()),
        "lease-counter",
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    );
    step_until_quiescent(&runner).await?;
    assert_eq!(counter.load(Ordering::SeqCst), 2, "both pings processed");
    runner.halt();
    Ok(())
}

/// A runner with a MockLeasor acquires the lease exactly once on the
/// first step.
#[tokio::test]
async fn runner_with_leasor_acquires_lease_on_first_step() -> Result<()> {
    let store = make_store();
    append_ping(&store);

    let (leasor, acquired) = MockLeasor::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let runner = ReactorRunner::new(
        Counter(counter.clone()),
        "lease-counter",
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_consumer_leasor(leasor as Arc<dyn ConsumerLeasor>);

    assert_eq!(acquired.load(Ordering::SeqCst), 0, "not acquired before first step");
    step_until_quiescent(&runner).await?;
    assert_eq!(acquired.load(Ordering::SeqCst), 1, "acquired exactly once on first step");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "ping processed after lease acquired");
    runner.halt();
    Ok(())
}

/// A runner whose leasor returns `Err` propagates the error from `step()`.
#[tokio::test]
async fn runner_with_failing_leasor_propagates_error_from_step() -> Result<()> {
    let store = make_store();
    let runner = ReactorRunner::new(
        Counter(Arc::new(AtomicUsize::new(0))),
        "lease-counter",
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_consumer_leasor(Arc::new(ErrorLeasor) as Arc<dyn ConsumerLeasor>);

    let err = runner.step(10).await.expect_err("step must fail when leasor errors");
    assert!(
        err.to_string().contains("lease acquisition failed"),
        "error must mention the leasor failure; got: {err:#}",
    );
    runner.halt();
    Ok(())
}

/// A blocking leasor: the second runner blocks until the first drops
/// its guard, then can proceed.
#[tokio::test(flavor = "multi_thread")]
async fn second_runner_blocks_until_first_drops_lease() -> Result<()> {
    use tokio::sync::{oneshot, Notify};

    // Leasor that serializes acquires: the first caller takes it immediately;
    // subsequent callers wait until it's released.
    struct BlockingLeasor {
        held: Arc<AtomicBool>,
        notify: Arc<Notify>,
    }

    struct BlockingGuard {
        held: Arc<AtomicBool>,
        notify: Arc<Notify>,
    }

    impl LeaseGuard for BlockingGuard {}
    impl Drop for BlockingGuard {
        fn drop(&mut self) {
            self.held.store(false, Ordering::SeqCst);
            self.notify.notify_waiters();
        }
    }

    #[async_trait]
    impl ConsumerLeasor for BlockingLeasor {
        async fn acquire(&self, _: &str) -> Result<Box<dyn LeaseGuard>> {
            while self.held.load(Ordering::SeqCst) {
                self.notify.notified().await;
            }
            self.held.store(true, Ordering::SeqCst);
            Ok(Box::new(BlockingGuard {
                held: self.held.clone(),
                notify: self.notify.clone(),
            }))
        }
    }

    let held = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let leasor: Arc<dyn ConsumerLeasor> = Arc::new(BlockingLeasor {
        held: held.clone(),
        notify: notify.clone(),
    });

    let store = Arc::new(MemoryStore::new());
    append_ping(&store);

    // First runner: acquires lease, processes the ping.
    let counter_a = Arc::new(AtomicUsize::new(0));
    let runner_a = ReactorRunner::new(
        Counter(counter_a.clone()),
        "blocked-consumer",
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_consumer_leasor(leasor.clone());

    step_until_quiescent(&runner_a).await?;
    assert_eq!(counter_a.load(Ordering::SeqCst), 1);
    assert!(held.load(Ordering::SeqCst), "runner A holds the lease after quiesce");

    // Second runner: should block on acquire until runner A halts.
    let store2 = store.clone();
    let leasor2 = leasor.clone();
    let counter_b = Arc::new(AtomicUsize::new(0));
    let counter_b2 = counter_b.clone();
    let (tx, rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let runner_b = ReactorRunner::new(
            Counter(counter_b2),
            "blocked-consumer",
            store2.clone() as Arc<dyn EventLogBackend>,
            store2.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_consumer_leasor(leasor2);
        // step() will block on acquire until runner A releases
        runner_b.step(256).await.expect("runner B step after lease acquired");
        let _ = tx.send(());
        runner_b.halt();
    });

    // Runner B must NOT have acquired yet (runner A still holds it).
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(counter_b.load(Ordering::SeqCst), 0, "runner B blocked while A holds");

    // Halt runner A — guard drops, lease released.
    runner_a.halt();
    drop(runner_a);

    // Runner B should now proceed within a short timeout.
    timeout(Duration::from_secs(5), rx)
        .await
        .expect("runner B must unblock after A releases the lease")
        .expect("channel should not be dropped");

    Ok(())
}

/// `EngineBuilder::with_consumer_leasor` must thread the leasor to EVERY
/// consumer it builds — reactors AND projectors. Each consumer acquires
/// its own lease (one per distinct consumer_id) on its first supervised
/// step. Before this wiring existed the builder dropped the leasor on the
/// floor and the count stayed 0.
#[tokio::test(flavor = "multi_thread")]
async fn builder_threads_leasor_to_projector_and_reactor() -> Result<()> {
    let store = make_store();
    let (leasor, acquired) = MockLeasor::new();

    let _engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_projector(PingProjector(Arc::new(AtomicUsize::new(0))))
    .with_reactor(Counter(Arc::new(AtomicUsize::new(0))))
    .allow_in_memory_effect_store_for_tests()
    .with_consumer_leasor(leasor as Arc<dyn ConsumerLeasor>)
    .build()
    .await?;

    // Both consumers acquire their lease on the first supervised step,
    // even with no events in the log (ensure_leased runs before the
    // cursor read). Poll until both have, with a bounded timeout.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while acquired.load(Ordering::SeqCst) < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "expected both consumers to acquire a lease; got {}",
            acquired.load(Ordering::SeqCst),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        acquired.load(Ordering::SeqCst),
        2,
        "exactly one acquire per consumer (projector + reactor)",
    );
    Ok(())
}

/// Acceptance: two engines with the same projector consumer name cannot
/// process concurrently. Driven at the runner level with a BlockingLeasor
/// (the engine-level wiring is covered above): the second
/// `ProjectionRunner` blocks on `step` until the first releases its lease.
#[tokio::test(flavor = "multi_thread")]
async fn second_projector_blocks_until_first_drops_lease() -> Result<()> {
    use tokio::sync::{oneshot, Notify};

    struct BlockingLeasor {
        held:   Arc<AtomicBool>,
        notify: Arc<Notify>,
    }
    struct BlockingGuard {
        held:   Arc<AtomicBool>,
        notify: Arc<Notify>,
    }
    impl LeaseGuard for BlockingGuard {}
    impl Drop for BlockingGuard {
        fn drop(&mut self) {
            self.held.store(false, Ordering::SeqCst);
            self.notify.notify_waiters();
        }
    }
    #[async_trait]
    impl ConsumerLeasor for BlockingLeasor {
        async fn acquire(&self, _: &str) -> Result<Box<dyn LeaseGuard>> {
            while self.held.load(Ordering::SeqCst) {
                self.notify.notified().await;
            }
            self.held.store(true, Ordering::SeqCst);
            Ok(Box::new(BlockingGuard {
                held:   self.held.clone(),
                notify: self.notify.clone(),
            }))
        }
    }

    let held = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let leasor: Arc<dyn ConsumerLeasor> = Arc::new(BlockingLeasor {
        held:   held.clone(),
        notify: notify.clone(),
    });

    let store = make_store();
    append_ping(&store);

    let count_a = Arc::new(AtomicUsize::new(0));
    let runner_a = ProjectionRunner::new(
        PingProjector(count_a.clone()),
        "lease-projector",
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
    )
    .with_consumer_leasor(leasor.clone());

    // Runner A acquires the lease and processes the ping.
    runner_a.step(256).await?;
    assert_eq!(count_a.load(Ordering::SeqCst), 1, "A processed the ping");
    assert!(held.load(Ordering::SeqCst), "A holds the lease");

    // Runner B (same consumer name) must block on step until A releases.
    let store2 = store.clone();
    let leasor2 = leasor.clone();
    let count_b = Arc::new(AtomicUsize::new(0));
    let count_b2 = count_b.clone();
    let (tx, rx) = oneshot::channel::<()>();
    let runner_b = ProjectionRunner::new(
        PingProjector(count_b2),
        "lease-projector",
        store2.clone() as Arc<dyn EventLogBackend>,
        store2.clone() as Arc<dyn CheckpointStore>,
    )
    .with_consumer_leasor(leasor2);
    tokio::spawn(async move {
        runner_b.step(256).await.expect("B step after lease acquired");
        let _ = tx.send(());
    });

    // While A holds, B must not have acquired/processed.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(count_b.load(Ordering::SeqCst), 0, "B blocked while A holds");

    // Drop A → guard drops → lease released.
    drop(runner_a);

    timeout(Duration::from_secs(5), rx)
        .await
        .expect("B must unblock after A releases the lease")
        .expect("channel should not be dropped");
    Ok(())
}
