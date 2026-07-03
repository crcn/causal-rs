//! Audit v111: does a transient read error during EngineBuilder::build's
//! cancel-fence rehydration (engine.rs `if let Ok(markers) = read_stream(...)`)
//! resurrect a durably-cancelled workflow for the process lifetime?
//!
//! Scenario:
//!   Boot 1: cancel wf X (durable marker on causal:control); the runner scans
//!           the marker and persists its ack-floor PAST it. Shutdown.
//!   Then a trigger for X lands in the log (position above the checkpoint).
//!   Boot 2: the control-stream read errors ONCE (transient blip). Build
//!           swallows the error, fence starts empty. The runner resumes at its
//!           checkpoint — ABOVE the marker — so it never re-learns the fence.
//!
//! CORRECT behavior: the trigger must NOT reach the reactor body (cancellation
//! is documented as durable across restarts). The assertion is written so that
//! correct behavior passes; the defect makes it fail.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::types::{EventData, LogCursor, RecordedEvent, StreamRevision, StreamState, WriteResult};
use causal::EngineBuilder;

// ─────────────────────────────────────────────────────────────────────
// Log wrapper: delegates to MemoryStore, but returns Err ONCE on
// read_stream of the control stream — a transient boot-time blip.
// ─────────────────────────────────────────────────────────────────────

struct BlipLog {
    inner: Arc<MemoryStore>,
    armed: Mutex<bool>,
}

impl BlipLog {
    fn new(inner: Arc<MemoryStore>, armed: bool) -> Arc<Self> {
        Arc::new(Self { inner, armed: Mutex::new(armed) })
    }
}

#[async_trait]
impl EventLogBackend for BlipLog {
    async fn read_all(&self, after: LogCursor, limit: usize) -> Result<Vec<RecordedEvent>> {
        EventLogBackend::read_all(self.inner.as_ref(), after, limit).await
    }
    async fn read_stream(
        &self,
        category: &str,
        subject_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        if category == "causal:control" {
            let mut armed = self.armed.lock();
            if *armed {
                *armed = false;
                return Err(anyhow!("transient storage blip reading control stream"));
            }
        }
        EventLogBackend::read_stream(self.inner.as_ref(), category, subject_id, after).await
    }
    async fn latest_position(&self) -> Result<LogCursor> {
        EventLogBackend::latest_position(self.inner.as_ref()).await
    }
    async fn append_to_stream(
        &self,
        category: &str,
        subject_id: Uuid,
        expected: StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult> {
        EventLogBackend::append_to_stream(
            self.inner.as_ref(), category, subject_id, expected, events,
        ).await
    }
}

// ─────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kick {
    id: Uuid,
}
impl Event for Kick {
    const NAME: &'static str = "kick";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Clone)]
struct Probe {
    ran: Arc<AtomicUsize>,
}
#[async_trait]
impl Reactor for Probe {
    type Trigger = Kick;
    const NAME: &'static str = "audit.probe";
    async fn react(&self, _t: &Kick, _ctx: Ctx<'_>) -> Result<Events> {
        self.ran.fetch_add(1, AtomicOrd::SeqCst);
        Ok(Events::new())
    }
}

/// Raw trigger append into workflow `wf` — the shape Engine::emit writes.
async fn append_kick(store: &MemoryStore, wf: Uuid) {
    let payload = Kick { id: Uuid::new_v4() };
    let ev = EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id: wf,
        event_type: <Kick as Event>::NAME.to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        created_at: Utc::now(),
        category: Some(<Kick as Event>::NAME.to_string()),
        subject_id: Some(payload.id),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    causal::append_event(store, ev).await.unwrap();
}

async fn boot(
    store: &Arc<MemoryStore>,
    log: Arc<dyn EventLogBackend>,
    ran: Arc<AtomicUsize>,
) -> causal::Engine {
    EngineBuilder::new(
        log,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_reactor(Probe { ran })
    .allow_in_memory_effect_store_for_tests()
    .allow_in_memory_decision_store_for_tests()
    .build()
    .await
    .expect("engine build must succeed")
}

/// Poll the reactor's durable cursor until it reaches `at_least`.
async fn wait_for_checkpoint(store: &MemoryStore, at_least: LogCursor, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let c = CheckpointStore::get(store, Probe::NAME).await.unwrap();
        if c.map_or(false, |c| c >= at_least) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for checkpoint >= {:?} ({what}); last = {:?}",
            at_least,
            c,
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Shared scenario. `blip` arms the one-shot control-stream read error at
/// Boot 2. Returns how many times the reactor body ran across both boots.
async fn cancelled_wf_across_restart(blip: bool) -> usize {
    let store = Arc::new(MemoryStore::new());
    let wf = Uuid::new_v4();
    let ran = Arc::new(AtomicUsize::new(0));

    // ── Boot 1: cancel wf; the runner scans the durable marker and its
    // ack-floor persists past it. Then "crash" (shutdown).
    let engine1 = boot(&store, store.clone() as Arc<dyn EventLogBackend>, ran.clone()).await;
    engine1.cancel_workflow(wf).await.unwrap();
    let marker_pos = EventLogBackend::latest_position(store.as_ref()).await.unwrap();
    wait_for_checkpoint(&store, marker_pos, "boot-1 floor past cancel marker").await;
    engine1.shutdown().await.unwrap();

    // A trigger for the CANCELLED workflow lands after the crash — at a
    // position above the persisted checkpoint, below nothing. (At-least-once
    // producers, queue redeliveries, or an in-flight peer can all do this.)
    append_kick(&store, wf).await;
    let trigger_pos = EventLogBackend::latest_position(store.as_ref()).await.unwrap();
    assert!(trigger_pos > marker_pos, "trigger must sit above the marker");

    // ── Boot 2: same durable state; control-stream read blips once iff armed.
    let log2 = BlipLog::new(store.clone(), blip);
    let engine2 = boot(&store, log2 as Arc<dyn EventLogBackend>, ran.clone()).await;
    // Either way the runner acks the trigger (fence-ack or body run), so the
    // floor reaches trigger_pos — wait for that, then inspect the body count.
    wait_for_checkpoint(&store, trigger_pos, "boot-2 floor past trigger").await;
    engine2.shutdown().await.unwrap();

    ran.load(AtomicOrd::SeqCst)
}

/// Control: with a healthy control-stream read at boot, the fence rehydrates
/// and the trigger is fence-acked without running the body. (Validates the
/// harness: the reactor CAN see the trigger, and only the fence stops it.)
#[tokio::test]
async fn control_healthy_boot_keeps_cancellation_durable() {
    let ran = tokio::time::timeout(
        Duration::from_secs(30),
        cancelled_wf_across_restart(false),
    )
    .await
    .expect("control scenario wedged");
    assert_eq!(
        ran, 0,
        "healthy boot: trigger into a cancelled workflow must never reach the body",
    );
}

/// The defect probe: ONE transient read error on the control stream at boot
/// must not un-cancel a durably-cancelled workflow. cancel_workflow's docs
/// promise the marker is 'durable across restarts'.
#[tokio::test]
async fn transient_boot_blip_must_not_resurrect_cancelled_workflow() {
    let ran = tokio::time::timeout(
        Duration::from_secs(30),
        cancelled_wf_across_restart(true),
    )
    .await
    .expect("blip scenario wedged");
    assert_eq!(
        ran, 0,
        "DEFECT: a transient control-stream read error at boot emptied the \
         cancel fence; the runner resumed above the marker and never re-learned \
         it — the cancelled workflow's trigger reached the reactor body \
         (ran {ran} time(s)). Cancellation must be durable across restarts.",
    );
}
