//! End-to-end reactor crash-recovery test with `PgEffectStore`.
//!
//! Verifies that `ctx.effect()` results survive a simulated process crash:
//! a reactor processes a trigger and caches its external-call result in
//! `PgEffectStore`, then crashes before the ack-floor checkpoint is written.
//! A fresh runner redelivers the same trigger — `PgEffectStore` serves the
//! cached result so the external call does NOT fire a second time.
//!
//! This is the production crash model: the window between effect execution
//! and checkpoint persistence is closed by the durable store.
//!
//!     DATABASE_URL=postgres://localhost/causal cargo test -p causal_replay \
//!         --features postgres --test pg_crash_recovery_test -- --ignored
#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::effect_store::EffectStore;
use causal::event::Event;
use causal::event_log::{append_event, EventLogBackend};
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::reactor_runner::ReactorRunner;
use causal::types::{EventData, LogCursor};
use causal_replay::effect_store::PgEffectStore;

// ── DB connection ─────────────────────────────────────────────────────

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

// ── Fault-injecting checkpoint ────────────────────────────────────────
//
// Wraps MemoryStore and fails set() exactly once, simulating a crash
// between the reactor completing and the floor checkpoint being written.
// After the fault fires, subsequent set() calls pass through normally.

struct FaultingCheckpoint {
    inner: Arc<MemoryStore>,
    fail_next_set: Mutex<bool>,
}

impl FaultingCheckpoint {
    fn new(inner: Arc<MemoryStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_next_set: Mutex::new(true),
        })
    }
}

#[async_trait]
impl CheckpointStore for FaultingCheckpoint {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        CheckpointStore::get(self.inner.as_ref(), consumer_id).await
    }
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        let should_fail = {
            let mut g = self.fail_next_set.lock();
            if *g { *g = false; true } else { false }
        };
        if should_fail {
            return Err(anyhow::anyhow!("fault: simulated crash at checkpoint write"));
        }
        CheckpointStore::set(self.inner.as_ref(), consumer_id, pos).await
    }
}

#[async_trait]
impl ReactorCheckpoint for FaultingCheckpoint {
    async fn record_reactor_attempt(
        &self, consumer_id: &str, trigger_id: Uuid,
    ) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, trigger_id).await
    }
    async fn clear_reactor_attempts(
        &self, consumer_id: &str, trigger_id: Uuid,
    ) -> Result<()> {
        self.inner.clear_reactor_attempts(consumer_id, trigger_id).await
    }
}

// ── Test event ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrashTestPing {
    id: Uuid,
    occurred_at: DateTime<Utc>,
}

impl Event for CrashTestPing {
    const NAME: &'static str = "pg_crash_test_ping";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

async fn append_crash_ping(store: &MemoryStore) {
    let payload = CrashTestPing { id: Uuid::new_v4(), occurred_at: Utc::now() };
    let ev = EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: "pg_crash_test_ping".to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        created_at: Utc::now(),
        category: Some("pg_crash_test_ping".to_string()),
        subject_id: Some(payload.id),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    append_event(store, ev).await.unwrap();
}

// ── Reactor ───────────────────────────────────────────────────────────

struct CrashTestReactor {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Reactor for CrashTestReactor {
    type Trigger = CrashTestPing;
    const NAME: &'static str = "pg-crash-test";

    async fn react(&self, _t: &CrashTestPing, ctx: Ctx<'_>) -> Result<Events> {
        let count = self.calls.clone();
        // ctx.effect() memoizes this call. On redelivery the closure must
        // NOT run — PgEffectStore should return the previously cached result.
        let _: serde_json::Value = ctx.effect("external-call", move || async move {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!("called"))
        })
        .await?;
        Ok(Events::new())
    }
}

// ── Test ──────────────────────────────────────────────────────────────

/// A reactor crash between side-effect execution and checkpoint write
/// must not re-execute the side effect when the trigger is redelivered.
///
/// Crash model:
/// 1. Runner A processes trigger T; `ctx.effect("external-call", …)` fires
///    (call_count → 1) and stores the result in `PgEffectStore`.
/// 2. The checkpoint write is fault-injected to fail — simulating a kill -9
///    between the effect persisting and the floor advancing.
/// 3. Runner B starts with a fresh MemoryStore checkpoint (position 0).
///    Trigger T is redelivered; `PgEffectStore` returns the cached result —
///    the closure does NOT run again (call_count stays at 1).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn pg_effect_store_prevents_duplicate_calls_after_crash() -> Result<()> {
    let pool = connect_local().await;
    let call_count = Arc::new(AtomicUsize::new(0));
    // Unique consumer per test run — avoids cross-run checkpoint pollution.
    let consumer_id = format!("pg-crash-test.{}", Uuid::new_v4());

    // Shared event log (MemoryStore — stable within this process).
    let log = Arc::new(MemoryStore::new());
    append_crash_ping(log.as_ref()).await;

    // PgEffectStore — the durable component under test.
    let pg_store = Arc::new(PgEffectStore::new(pool.clone()));

    // ── Runner A: crashes before checkpoint write ─────────────────────
    //
    // The fault fires on the first checkpoint.set() call, which happens
    // in advance_floor() AFTER react() has returned and PgEffectStore.put()
    // has been called. At that point the external call is durably cached.

    let faulty_cp = FaultingCheckpoint::new(Arc::new(MemoryStore::new()));
    let runner_a = ReactorRunner::new(
        CrashTestReactor { calls: call_count.clone() },
        &consumer_id,
        log.clone() as Arc<dyn EventLogBackend>,
        faulty_cp.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_effect_store(pg_store.clone() as Arc<dyn EffectStore>);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match runner_a.step(10).await {
            Err(_) => break,  // fault fired — "process crashed"
            Ok(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fault never fired within 5 s"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
    assert_eq!(
        call_count.load(Ordering::SeqCst), 1,
        "external call must have fired exactly once before the crash"
    );
    runner_a.halt();

    // ── Runner B: fresh checkpoint, same PgEffectStore ────────────────
    //
    // Starts from position 0 (no prior checkpoint) — trigger T is
    // redelivered. PgEffectStore serves the cached result; the closure
    // must not execute.

    let fresh_cp = Arc::new(MemoryStore::new());
    let runner_b = ReactorRunner::new(
        CrashTestReactor { calls: call_count.clone() },
        &consumer_id,
        log.clone() as Arc<dyn EventLogBackend>,
        fresh_cp.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_effect_store(pg_store.clone() as Arc<dyn EffectStore>);

    // Step until the checkpoint advances (trigger T processed and acked).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while fresh_cp.get(&consumer_id).await?.is_none() {
        runner_b.step(10).await?;
        assert!(
            std::time::Instant::now() < deadline,
            "runner B never advanced checkpoint"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    assert_eq!(
        call_count.load(Ordering::SeqCst), 1,
        "PgEffectStore must have served the cached result — external call must NOT run again"
    );
    runner_b.halt();
    Ok(())
}
