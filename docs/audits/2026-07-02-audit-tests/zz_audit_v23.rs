//! Audit probe for finding #23: `Engine::settle`'s `drained()` probe does
//! durable checkpoint I/O (reap -> advance_floor -> checkpoint.advance) and
//! propagates a single transient store error as a hard, untyped settle
//! failure — while the exact same error inside the runner's own step is
//! infra-retried forever (supervise_one backoff; A4 policy).
//!
//! Assertion direction: CORRECT behavior = settle tolerates a transient
//! blip while the chain drains normally. The defect makes the first
//! settle() return Err (the raw injected error), failing the test.

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use causal::{
    CheckpointStore, Ctx, EngineBuilder, Event, Events, LogCursor, MemoryStore,
    Ordering, Reactor, ReactorCheckpoint,
};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kick {
    id: Uuid,
}
impl Event for Kick {
    const NAME: &'static str = "kick";
    const SUBJECT: &'static str = "k";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Boom {
    id: Uuid,
}
impl Event for Boom {
    const NAME: &'static str = "boom";
    const SUBJECT: &'static str = "k";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

/// Healthy reactor: Kick -> Boom, instantly. Nothing about the CHAIN is
/// wrong in this test; only the checkpoint backend blips.
struct Kicker;
#[async_trait]
impl Reactor for Kicker {
    type Trigger = Kick;
    const NAME: &'static str = "kicker";
    const ORDERING: Ordering = Ordering::PerWorkflow;
    async fn react(&self, t: &Kick, _ctx: Ctx<'_>) -> Result<Events> {
        let mut out = Events::new();
        out.push(Boom { id: t.id });
        Ok(out)
    }
}

/// Checkpoint wrapper injecting failures on `advance` while armed —
/// a transient backend blip (pool hiccup). Everything else delegates.
struct FlakyCheckpoint {
    inner: Arc<MemoryStore>,
    fail_advance: AtomicBool,
}

#[async_trait]
impl CheckpointStore for FlakyCheckpoint {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        self.inner.get(consumer_id).await
    }
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        self.inner.set(consumer_id, pos).await
    }
    async fn advance(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        if self.fail_advance.load(AtomicOrd::SeqCst) {
            anyhow::bail!("injected transient checkpoint blip (pool hiccup)");
        }
        self.inner.advance(consumer_id, pos).await
    }
    async fn list_consumers(&self) -> Result<Vec<String>> {
        self.inner.list_consumers().await
    }
    async fn clamp_ahead_of(&self, tip: LogCursor) -> Result<u64> {
        self.inner.clamp_ahead_of(tip).await
    }
}

#[async_trait]
impl ReactorCheckpoint for FlakyCheckpoint {
    async fn record_reactor_attempt(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, trigger_id).await
    }
    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<()> {
        self.inner.clear_reactor_attempts(consumer_id, trigger_id).await
    }
}

#[tokio::test]
async fn settle_survives_transient_checkpoint_blip() {
    let store = Arc::new(MemoryStore::new());
    let flaky = Arc::new(FlakyCheckpoint {
        inner: store.clone(),
        fail_advance: AtomicBool::new(false),
    });

    let engine = EngineBuilder::new(store.clone(), store.clone(), flaky.clone())
        .allow_in_memory_effect_store_for_tests()
        .allow_in_memory_decision_store_for_tests()
        .with_reactor(Kicker)
        .build()
        .await
        .unwrap();

    let run = Uuid::new_v4();

    // Arm the blip BEFORE the chain starts: the first ack-floor persist —
    // whether driven by the supervisor's step (infra-retried, tolerated)
    // or by settle's drained() probe (propagated, the defect) — fails
    // like a transient pool hiccup.
    flaky.fail_advance.store(true, AtomicOrd::SeqCst);

    let result = engine.emit(Kick { id: run }).workflow_id(run).await.unwrap();

    let first = timeout(Duration::from_secs(10), engine.settle(result))
        .await
        .expect("settle never returned — a wedge, not the claimed abort");

    // Blip over.
    flaky.fail_advance.store(false, AtomicOrd::SeqCst);

    // Prove the chain was healthy the whole time: with the blip gone, the
    // same workflow settles cleanly (nothing was lost or parked).
    let second = timeout(Duration::from_secs(10), engine.settle(result))
        .await
        .expect("re-settle wedged");
    assert!(
        second.is_ok(),
        "chain failed to drain even after the blip cleared: {:?}",
        second.err()
    );

    engine.shutdown().await.unwrap();

    // THE claim under test. Correct behavior: settle, like the runner's
    // own infra-retry arm, rides out a transient probe blip. The defect:
    // settle aborts with the raw injected error while work continues.
    assert!(
        first.is_ok(),
        "DEFECT CONFIRMED: settle() aborted on a transient checkpoint blip \
         while the chain drained normally. Propagated error: {:#}",
        first.unwrap_err()
    );
}
