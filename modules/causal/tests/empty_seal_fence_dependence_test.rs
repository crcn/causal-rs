//! Audit verifier test for finding #113.
//!
//! Claim: with `.seal_empty_decisions(false)` (A6 elision), a reactor body
//! that returns `Ok(Events::new())` because `ctx.is_workflow_cancelled()`
//! was true seals NOTHING — so the ∅ decision has no durable trace. A
//! redelivery in a context where the cancel fence reads false (the builder's
//! fence rehydration is fail-open: engine.rs `if let Ok(markers)` swallows a
//! storage blip and starts with an EMPTY fence; a lagging scan has not yet
//! reached the cancel marker) get-misses the decision replay gate, re-runs
//! the body, and seals+appends the FULL batch. One trigger, two outcomes.
//!
//! Test 1 asserts the library promise (one decision per trigger) and is
//! EXPECTED TO FAIL if the defect is real.
//! Test 2 is the control: identical scenario with the default
//! `seal_empty_decisions(true)` — the empty record is sealed, the replay
//! gate hits, the body is never re-run. Expected to PASS, isolating the
//! elision flag as the enabler.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::decision_store::{DecisionStore, InMemoryDecisionStore};
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::{Ctx, EngineBuilder, Event, Events, Reactor};
use causal::types::{EventData, LogCursor, RecordedEvent, StreamRevision, StreamState, WriteResult};

const CANCEL_MARKER_KIND: &str = "causal:workflow_cancelled";
const CONTROL_SUBJECT: &str = "causal:control";

// ── Facts ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditTrigger {
    run_id: Uuid,
}
impl Event for AuditTrigger {
    const NAME: &'static str = "audit_trigger";
    const SUBJECT: &'static str = "audit_run";
    fn subject_id(&self) -> Uuid {
        self.run_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditOutput {
    run_id: Uuid,
}
impl Event for AuditOutput {
    const NAME: &'static str = "audit_output";
    const SUBJECT: &'static str = "audit_run";
    fn subject_id(&self) -> Uuid {
        self.run_id
    }
}

// ── Reactor: the documented cooperative-cancel early-exit pattern ────

struct Gate {
    entered_tx: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
}

/// Body shape from contexts.rs docs: check `ctx.is_workflow_cancelled()`
/// and return `Ok(Events::new())` early; otherwise emit the real batch.
/// The optional gate (incarnation 1 only) parks the body mid-flight so the
/// test can land `cancel_workflow` after dispatch — the exact in-flight
/// window `Engine::cancel_workflow`'s own docs describe ("let
/// `Ctx::is_workflow_cancelled` return true for in-flight triggers that
/// were dispatched before this call").
struct FanOut {
    gate: Option<Gate>,
    calls: Arc<AtomicUsize>,
    observed_cancelled: Arc<Mutex<Vec<bool>>>,
}

#[async_trait]
impl Reactor for FanOut {
    type Trigger = AuditTrigger;
    const NAME: &'static str = "audit-fanout";

    async fn react(&self, t: &AuditTrigger, ctx: Ctx<'_>) -> Result<Events> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if let Some(g) = &self.gate {
            let _ = g.entered_tx.send(());
            let permit = g
                .release
                .acquire()
                .await
                .map_err(|e| anyhow!("gate closed: {e}"))?;
            permit.forget();
        }
        let cancelled = ctx.is_workflow_cancelled();
        self.observed_cancelled.lock().unwrap().push(cancelled);
        if cancelled {
            return Ok(Events::new());
        }
        let mut out = Events::new();
        out.push(AuditOutput { run_id: t.run_id });
        Ok(out)
    }
}

// ── Wrapper A: crash-before-floor-persist (incarnation 1) ────────────
//
// Delegates everything to the shared MemoryStore, but once armed, every
// checkpoint write fails — the moral kill -9 between the body finishing
// and the ack-floor persisting (same modeling as crash_injection.rs's
// CheckpointSet fault, held down instead of one-shot).

struct CrashCheckpoint {
    inner: Arc<MemoryStore>,
    block_sets: AtomicBool,
}

#[async_trait]
impl EventLogBackend for CrashCheckpoint {
    async fn read_all(&self, after: LogCursor, limit: usize) -> Result<Vec<RecordedEvent>> {
        EventLogBackend::read_all(self.inner.as_ref(), after, limit).await
    }
    async fn read_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        EventLogBackend::read_stream(self.inner.as_ref(), aggregate_type, aggregate_id, after).await
    }
    async fn latest_position(&self) -> Result<LogCursor> {
        EventLogBackend::latest_position(self.inner.as_ref()).await
    }
    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult> {
        EventLogBackend::append_to_stream(self.inner.as_ref(), aggregate_type, aggregate_id, expected, events)
            .await
    }
}

#[async_trait]
impl CheckpointStore for CrashCheckpoint {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        CheckpointStore::get(self.inner.as_ref(), consumer_id).await
    }
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        if self.block_sets.load(AtomicOrdering::SeqCst) {
            return Err(anyhow!("audit fault: process crashed before floor persist"));
        }
        CheckpointStore::set(self.inner.as_ref(), consumer_id, pos).await
    }
}

#[async_trait]
impl ReactorCheckpoint for CrashCheckpoint {
    async fn record_reactor_attempt(&self, consumer_id: &str, trigger_id: Uuid) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, trigger_id).await
    }
    async fn clear_reactor_attempts(&self, consumer_id: &str, trigger_id: Uuid) -> Result<()> {
        self.inner.clear_reactor_attempts(consumer_id, trigger_id).await
    }
}

// ── Wrapper B: fence-blip + lagging scan (incarnation 2) ─────────────
//
// 1. `read_stream("causal:control", ..)` fails — the builder's fence
//    rehydration swallows the error (`if let Ok(markers)` in engine.rs)
//    and starts with an EMPTY fence. This is the library's own documented
//    fail-open ("Errors (stream absent, storage blip) are benign").
// 2. `read_all` hides cancel markers — models the redelivering instance
//    whose catch-up scan has not yet reached the marker while the
//    redelivered trigger (earlier in the log) is already executing.

struct LaggingLog {
    inner: Arc<MemoryStore>,
}

#[async_trait]
impl EventLogBackend for LaggingLog {
    async fn read_all(&self, after: LogCursor, limit: usize) -> Result<Vec<RecordedEvent>> {
        let evs = EventLogBackend::read_all(self.inner.as_ref(), after, limit).await?;
        Ok(evs
            .into_iter()
            .filter(|e| e.event_type != CANCEL_MARKER_KIND)
            .collect())
    }
    async fn read_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        // Models a lagging replica: incarnation 2's control stream has not
        // yet received the cancel marker, so fence rehydration reads it as
        // legitimately empty (NOT an error — a read fault now correctly
        // fails the build, slice 1). The fence therefore reads false on the
        // redelivering instance; only slice 3's sealed ∅ record (via the
        // replay gate) prevents the re-decision.
        let evs = EventLogBackend::read_stream(
            self.inner.as_ref(),
            aggregate_type,
            aggregate_id,
            after,
        )
        .await?;
        Ok(evs
            .into_iter()
            .filter(|e| e.event_type != CANCEL_MARKER_KIND)
            .collect())
    }
    async fn latest_position(&self) -> Result<LogCursor> {
        EventLogBackend::latest_position(self.inner.as_ref()).await
    }
    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult> {
        EventLogBackend::append_to_stream(self.inner.as_ref(), aggregate_type, aggregate_id, expected, events)
            .await
    }
}

// ── Shared scenario driver ───────────────────────────────────────────

async fn count_outputs(store: &MemoryStore) -> usize {
    EventLogBackend::read_all(store, LogCursor::ZERO, 1000)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.event_type == "audit_output")
        .count()
}

/// Runs the full crash-and-redeliver scenario. Returns
/// (outputs_in_log, body_calls, observed_cancelled_flags, sealed_consumers_after_inc1).
async fn run_scenario(seal_empty: bool) -> (usize, usize, Vec<bool>, Vec<String>) {
    let shared = Arc::new(MemoryStore::new());
    let ds = Arc::new(InMemoryDecisionStore::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));

    // ── Incarnation 1 ────────────────────────────────────────────────
    let crash = Arc::new(CrashCheckpoint {
        inner: shared.clone(),
        block_sets: AtomicBool::new(false),
    });
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));

    let engine1 = EngineBuilder::new(
        crash.clone() as Arc<dyn EventLogBackend>,
        crash.clone() as Arc<dyn CheckpointStore>,
        crash.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .allow_in_memory_effect_store_for_tests()
    .with_decision_store(ds.clone() as Arc<dyn DecisionStore>)
    .seal_empty_decisions(seal_empty)
    .with_reactor(FanOut {
        gate: Some(Gate {
            entered_tx,
            release: release.clone(),
        }),
        calls: calls.clone(),
        observed_cancelled: observed.clone(),
    })
    .build()
    .await
    .expect("engine1 build");

    let wf = Uuid::new_v4();
    let emit = tokio::time::timeout(
        Duration::from_secs(5),
        engine1
            .emit(AuditTrigger { run_id: Uuid::new_v4() })
            .workflow_id(wf),
    )
    .await
    .expect("emit timed out")
    .expect("emit failed");

    // Body is in flight (passed the dispatch gate and worker fence while
    // the fence was empty).
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("body never started — trigger not dispatched")
        .expect("gate channel closed");

    // Cancel lands after dispatch: durable marker appended, fence set.
    tokio::time::timeout(Duration::from_secs(5), engine1.cancel_workflow(wf))
        .await
        .expect("cancel timed out")
        .expect("cancel failed");

    // Crash point armed: floor can never persist past the trigger.
    crash.block_sets.store(true, AtomicOrdering::SeqCst);

    // Body resumes, observes cancelled=true, returns Ok(Events::new()).
    release.add_permits(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while observed.lock().unwrap().len() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "body did not complete after release"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        observed.lock().unwrap()[0],
        "first delivery must observe the cancel fence"
    );
    // Give the runner time to seal (or elide) and finish the attempt.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        count_outputs(&shared).await,
        0,
        "first delivery decided the EMPTY batch — nothing appended"
    );
    let sealed_after_inc1 = ds.list_consumers().await.unwrap();

    // Durable floor never passed the trigger — redelivery guaranteed.
    let floor = CheckpointStore::get(shared.as_ref(), "audit-fanout")
        .await
        .unwrap()
        .unwrap_or(LogCursor::ZERO);
    assert!(
        floor < emit.position,
        "crash injection failed: floor {floor:?} persisted past trigger {:?}",
        emit.position
    );

    // Crash: kill incarnation 1.
    let _ = tokio::time::timeout(Duration::from_secs(5), engine1.shutdown()).await;

    // ── Incarnation 2: lagging replica — the cancel marker has not yet
    //    replicated, so both the fence rehydration (read_stream) and the
    //    catch-up scan (read_all) see no marker. The fence reads false; only
    //    a durably-sealed ∅ decision can stop the body re-deciding. ───────
    let lag = Arc::new(LaggingLog { inner: shared.clone() });
    let engine2 = EngineBuilder::new(
        lag as Arc<dyn EventLogBackend>,
        shared.clone() as Arc<dyn CheckpointStore>,
        shared.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .allow_in_memory_effect_store_for_tests()
    .with_decision_store(ds.clone() as Arc<dyn DecisionStore>)
    .seal_empty_decisions(seal_empty)
    .with_reactor(FanOut {
        gate: None,
        calls: calls.clone(),
        observed_cancelled: observed.clone(),
    })
    .build()
    .await
    .expect("engine2 build (lagging replica: control stream reads empty)");

    // Wait for the redelivery to resolve: either the replay gate handled it
    // (correct) or the body re-ran and appended a full batch (defect).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if count_outputs(&shared).await > 0 {
            break; // defect materialized — no need to wait longer
        }
        if tokio::time::Instant::now() >= deadline {
            break; // quiet — correct behavior
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let outputs = count_outputs(&shared).await;
    let total_calls = calls.load(AtomicOrdering::SeqCst);
    let flags = observed.lock().unwrap().clone();
    let _ = tokio::time::timeout(Duration::from_secs(5), engine2.shutdown()).await;
    (outputs, total_calls, flags, sealed_after_inc1)
}

// ── Test 1: seal_empty_decisions(false) + a fence-consulting body. The
//    fix (slice 3, #113) makes fence-consulted emptiness NON-elidable, so
//    the empty decision seals despite the flag and the replay gate stops
//    the redelivery re-deciding. Asserts the library promise; FAILED before
//    the fix (empty decision elided → redelivery re-ran the body). ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fence_consulted_empty_cancel_decision_must_not_re_decide_on_redelivery() {
    let (outputs, calls, flags, sealed_after_inc1) = run_scenario(false).await;

    // The fix: the body called ctx.is_workflow_cancelled(), so its empty
    // return is fence-DEPENDENT, not deterministic — elision is overridden
    // and the empty decision seals durably. (Pre-fix this was empty: the
    // elision fired, leaving nothing for the redelivery to replay.)
    assert_eq!(
        sealed_after_inc1,
        vec!["audit-fanout".to_string()],
        "fence-consulted emptiness must seal even under seal_empty_decisions(false); \
         got {sealed_after_inc1:?}"
    );

    eprintln!(
        "AUDIT: outputs_in_log={outputs} body_calls={calls} observed_cancelled={flags:?}"
    );

    // THE PROMISE: one decision per trigger. The first delivery decided the
    // empty batch (cancel observed). A redelivery must never produce a
    // different outcome — no outputs may ever appear for this trigger.
    assert_eq!(
        outputs, 0,
        "CHIMERA: first delivery decided ∅ (cancel fence read true), redelivery \
         re-ran the body with an empty fence and appended a FULL batch — \
         two different outcomes for one trigger"
    );
}

// ── Test 2 (control): default seal_empty_decisions(true) — the empty
//    record is durable, the replay gate hits, no re-decision. ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_empty_cancel_decision_replays_empty_on_redelivery() {
    let (outputs, calls, flags, sealed_after_inc1) = run_scenario(true).await;

    assert_eq!(
        sealed_after_inc1,
        vec!["audit-fanout".to_string()],
        "default seals the empty decision durably"
    );

    eprintln!(
        "AUDIT-CONTROL: outputs_in_log={outputs} body_calls={calls} observed_cancelled={flags:?}"
    );

    assert_eq!(
        outputs, 0,
        "with the empty record sealed, redelivery must replay ∅, not re-decide"
    );
    assert_eq!(
        calls, 1,
        "replay gate must prevent the body from re-running"
    );
}
