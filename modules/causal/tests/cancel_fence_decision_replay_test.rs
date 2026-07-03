//! Audit verifier #12: the design doc promises "A sealed record for a
//! workflow cancelled *after* sealing still appends — the decision happened
//! pre-cancel; that is correct, not a leak"
//! (docs/plans/2026-07-02-decision-records-design.md, "Fence + H7" bullet).
//!
//! Claimed defect: both cancel-fence gates (dispatch gate and worker gate in
//! reactor_runner.rs) ack a redelivered trigger of a cancelled workflow
//! WITHOUT consulting the decision store, so a decision sealed before a
//! crash — with its append loop interrupted mid-batch — is never completed
//! if the workflow is cancelled while the process is down. The log then
//! permanently holds a strict subset of a sealed decision's outputs.
//!
//! Two tests:
//! - `control_...` (no cancel): redelivery replays the sealed record and
//!   completes the torn batch. Expected to PASS today — validates harness.
//! - `sealed_decision_...cancelled...` (cancel while down): per the design
//!   doc the batch must STILL complete. FAILS iff the defect is real.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event_log::EventLogBackend;
use causal::reactor::{Events, Reactor};
use causal::{
    DecisionRecord, DecisionStore, EngineBuilder, EventData, InMemoryDecisionStore,
    LogCursor, MemoryStore, StartPosition, StreamState,
};

#[causal::event(name = "zz12_order_shipped", subject_id = "order_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderShipped {
    order_id: Uuid,
}

/// Reactor body emits NOTHING — so any `zz12_note` row in the log can only
/// have come from replaying the (manually pre-sealed) decision record,
/// never from the body re-running.
struct Notify;
#[async_trait]
impl Reactor for Notify {
    type Trigger = OrderShipped;
    const NAME: &'static str = "zz12.notify";
    async fn react(&self, _t: &OrderShipped, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(Events::new())
    }
}

fn builder(store: &Arc<MemoryStore>, ds: &Arc<InMemoryDecisionStore>) -> EngineBuilder {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .allow_in_memory_effect_store_for_tests()
    .with_decision_store(ds.clone() as Arc<dyn DecisionStore>)
}

fn note_output(trigger_id: Uuid, wf: Uuid, subject: Uuid, n: u32) -> EventData {
    EventData {
        event_id: Uuid::new_v4(),
        causation_id: Some(trigger_id),
        workflow_id: wf,
        event_type: "zz12_note".to_string(),
        payload: serde_json::json!({ "n": n }),
        created_at: Utc::now(),
        category: Some("zz12_note".to_string()),
        subject_id: Some(subject),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

/// Shared scenario. Returns (appended `zz12_note` count, second-output
/// event_id present in log?).
///
/// 1. Emit trigger T (workflow W) with no reactor running.
/// 2. Manually seal decision R = [A, B] for (Notify::NAME, T) and append A
///    from the sealed canonical batch — exactly the state a `kill -9`
///    between the first and second append_outputs iteration leaves behind.
/// 3. Optionally cancel W (durable control-stream marker) — "while down".
/// 4. Restart: new engine with the reactor at StartPosition::Zero, same
///    log/checkpoint/decision stores. Wait until the reactor's ack-floor
///    checkpoint passes T (the trigger was scanned and acked).
/// 5. Count `zz12_note` rows.
async fn crash_mid_append_then_restart(cancel_while_down: bool) -> (usize, bool) {
    let store = Arc::new(MemoryStore::new());
    let ds = Arc::new(InMemoryDecisionStore::new());

    // Phase 1: the "before crash" process. No reactor registered — the body
    // never runs here; we install its sealed decision by hand below.
    let engine1 = builder(&store, &ds).build().await.unwrap();
    engine1
        .emit(OrderShipped { order_id: Uuid::new_v4() })
        .await
        .unwrap();

    let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 50)
        .await
        .unwrap();
    let trigger = all
        .iter()
        .find(|e| e.event_type == "zz12_order_shipped")
        .expect("trigger appended");
    let (trigger_id, wf, trigger_pos) = (trigger.event_id, trigger.workflow_id, trigger.position);

    // Seal R = [A, B]; append A from the sealed canonical batch (what the
    // runner's append loop does), then "crash" before B.
    let subject = Uuid::new_v4();
    let rec = DecisionRecord::new(
        Notify::NAME,
        trigger_id,
        trigger_pos,
        vec![
            note_output(trigger_id, wf, subject, 0),
            note_output(trigger_id, wf, subject, 1),
        ],
        Utc::now(),
    );
    let sealed = ds.seal(rec).await.unwrap();
    assert_eq!(sealed.outputs.len(), 2, "record sealed with two outputs");
    EventLogBackend::append_to_stream(
        store.as_ref(),
        "zz12_note",
        subject,
        StreamState::Any,
        vec![sealed.outputs[0].clone()],
    )
    .await
    .unwrap();
    let second_output_id = sealed.outputs[1].event_id;

    // The operator cancels W while the reactor process is down. The marker
    // is durable (control stream); the fence is rebuilt at build().
    if cancel_while_down {
        engine1.cancel_workflow(wf).await.unwrap();
    }
    engine1.shutdown().await.unwrap();

    // Phase 2: restart. The reactor scans from ZERO, so T is redelivered.
    let engine2 = builder(&store, &ds)
        .with_reactor_start(Notify, StartPosition::Zero)
        .build()
        .await
        .unwrap();

    // Wait for the reactor's durable ack-floor to pass the trigger: the
    // runner has scanned T and acked it (via replay OR via the fence gate).
    // Appends happen before the ack, so once the floor covers T the batch
    // is as complete as it will ever be.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let c = CheckpointStore::get(store.as_ref(), Notify::NAME)
                .await
                .unwrap()
                .unwrap_or(LogCursor::ZERO);
            if c >= trigger_pos {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reactor never scanned past the redelivered trigger (wedge)");
    // Small grace period so a completion racing the floor write can land.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 100)
        .await
        .unwrap();
    let notes: Vec<_> = all.iter().filter(|e| e.event_type == "zz12_note").collect();
    let second_present = notes.iter().any(|e| e.event_id == second_output_id);
    let count = notes.len();
    engine2.shutdown().await.unwrap();
    (count, second_present)
}

/// Control: no cancellation. Redelivery hits the decision-replay gate and
/// completes the torn batch. Passing this validates the harness.
#[tokio::test]
async fn control_sealed_decision_interrupted_mid_append_completes_on_redelivery() {
    let (count, second_present) = crash_mid_append_then_restart(false).await;
    assert!(
        second_present,
        "replay must append the un-appended second output (got {count} notes)"
    );
    assert_eq!(count, 2, "the sealed two-output batch is complete in the log");
}

/// The design-doc promise under test: "A sealed record for a workflow
/// cancelled *after* sealing still appends — the decision happened
/// pre-cancel; that is correct, not a leak."
#[tokio::test]
async fn sealed_decision_interrupted_mid_append_still_completes_when_workflow_cancelled_while_down()
{
    let (count, second_present) = crash_mid_append_then_restart(true).await;
    assert!(
        second_present,
        "DEFECT: the sealed decision's second output was never appended — the \
         cancel fence acked the redelivered trigger without consulting the \
         decision store, leaving a permanently torn batch ({count} of 2 \
         sealed outputs in the log)"
    );
    assert_eq!(count, 2, "the sealed two-output batch is complete in the log");
}
