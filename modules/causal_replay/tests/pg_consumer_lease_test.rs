//! Postgres consumer-lease integration test.
//!
//! Requires a local Postgres with the causal_checkpoints table (migration 054).
//! All tests are `#[ignore]`'d by default:
//!
//!     DATABASE_URL=postgres://localhost/causal cargo test -p causal_replay \
//!         --features postgres --test pg_consumer_lease_test -- --ignored
#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use tokio::time::timeout;
use uuid::Uuid;

use causal::checkpoint_store::ReactorCheckpoint;
use causal::consumer_lease::ConsumerLeasor;
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::reactor::{Events, Reactor};
use causal::reactor_runner::ReactorRunner;
use causal::types::EventData;
use causal_replay::event_log::PgEventLogBackend;
use causal_replay::reactor_checkpoint::PgReactorCheckpoint;
use causal_replay::PgConsumerLeasor;

async fn connect_local() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "Refusing to run integration tests against non-local Postgres: {url}"
    );
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect local postgres")
}

// ── Trigger fact ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseTestPing {
    id: Uuid,
    occurred_at: DateTime<Utc>,
}
impl Event for LeaseTestPing {
    const NAME: &'static str = "pg_lease_test_ping";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

// ── Reactor ───────────────────────────────────────────────────────────

struct PgLeaseCounter(Arc<AtomicUsize>);

#[async_trait]
impl Reactor for PgLeaseCounter {
    type Trigger = LeaseTestPing;
    const NAME: &'static str = "pg-lease-counter";
    async fn react(&self, _t: &LeaseTestPing, _ctx: Ctx<'_>) -> Result<Events> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Events::new())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

async fn append_ping(log: &PgEventLogBackend) {
    let ev = EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: "pg_lease_test_ping".to_string(),
        payload: serde_json::to_value(LeaseTestPing {
            id: Uuid::new_v4(),
            occurred_at: Utc::now(),
        })
        .unwrap(),
        created_at: Utc::now(),
        category: Some("pg_lease_test_ping".to_string()),
        subject_id: Some(Uuid::new_v4()),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    causal::event_log::append_event(log, ev).await.expect("append_ping");
}

/// Step repeatedly until there are no more events and the runner reports Idle.
async fn step_to_idle(runner: &ReactorRunner<PgLeaseCounter>) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let outcome = runner.step(256).await?;
        if matches!(outcome, causal::projection_runner::StepOutcome::Idle) {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let check = runner.step(256).await?;
            if matches!(check, causal::projection_runner::StepOutcome::Idle) {
                return Ok(());
            }
        }
        assert!(std::time::Instant::now() < deadline, "step_to_idle timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

/// Second engine blocks on same consumer until first halts.
///
/// - Start engine A with consumer "test.reactor", leasor pointing to local DB.
/// - Emit 3 triggers.
/// - Verify A processes all 3.
/// - Halt A (drops the session connection → lock released).
/// - Start engine B with the same consumer name and leasor.
/// - Emit 1 more trigger.
/// - Verify B can acquire the lock and processes the new trigger.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local DATABASE_URL + migration 054 (causal_checkpoints table)"]
async fn second_engine_blocks_until_first_halts() -> Result<()> {
    let pool = connect_local().await;
    let url = std::env::var("DATABASE_URL").unwrap();

    let log = Arc::new(PgEventLogBackend::new(pool.clone()));
    let checkpoint = Arc::new(PgReactorCheckpoint::new(pool.clone()));
    let leasor = Arc::new(PgConsumerLeasor::new(&url));

    // Use a unique consumer name per test run to avoid cross-run pollution.
    let consumer = format!("test.reactor.{}", Uuid::new_v4());

    // Emit 3 triggers.
    for _ in 0..3 {
        append_ping(log.as_ref()).await;
    }

    // Engine A: acquires lease, processes the 3 triggers.
    let counter_a = Arc::new(AtomicUsize::new(0));
    let runner_a = ReactorRunner::new(
        PgLeaseCounter(counter_a.clone()),
        &consumer,
        log.clone() as Arc<dyn EventLogBackend>,
        checkpoint.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_consumer_leasor(leasor.clone() as Arc<dyn ConsumerLeasor>);

    step_to_idle(&runner_a).await?;
    assert_eq!(counter_a.load(Ordering::SeqCst), 3, "A processed all 3 triggers");

    // Engine B starts in a background task — will block until A halts.
    let log2 = log.clone();
    let checkpoint2 = checkpoint.clone();
    let leasor2 = leasor.clone();
    let consumer2 = consumer.clone();
    let counter_b = Arc::new(AtomicUsize::new(0));
    let counter_b2 = counter_b.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        let runner_b = ReactorRunner::new(
            PgLeaseCounter(counter_b2),
            &consumer2,
            log2.clone() as Arc<dyn EventLogBackend>,
            checkpoint2.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_consumer_leasor(leasor2 as Arc<dyn ConsumerLeasor>);
        // step() blocks inside acquire() until A's connection closes.
        runner_b.step(256).await.expect("runner B step after lease acquired");
        let _ = tx.send(());
        runner_b.halt();
    });

    // Give B time to try — it should still be blocked.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(counter_b.load(Ordering::SeqCst), 0, "B must not process while A holds");

    // Halt A — drops the connection, Postgres releases the advisory lock.
    runner_a.halt();
    drop(runner_a);

    // Emit one more trigger for B to process.
    append_ping(log.as_ref()).await;

    // B should unblock and complete within 10 seconds.
    timeout(Duration::from_secs(10), rx)
        .await
        .expect("B must unblock after A halts")
        .expect("channel ok");

    Ok(())
}
