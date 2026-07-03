//! Audit verification for FINDING #7: json_extract_id swallows
//! deserialization failures with `.ok()?`, so a payload that fails
//! aggregate deserialization is silently excluded from every live fold,
//! contradicting the documented "bad payload is fatal" contract
//! (aggregator.rs step 4 doc: "an apply error **fails the fold** ...
//! live fold and replay agree that a bad payload is fatal") and the
//! projection runner's fold-poison park path (projection_runner.rs:256).
//!
//! CORRECT behavior passes these tests; the defect makes them FAIL.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::aggregator::{Aggregator, AggregatorRegistry};
use causal::projection_failure::PROJECTION_FAILED_KIND;
use causal::projection_runner::ProjectionRunner;
use causal::{
    append_event, Aggregate, Apply, Ctx, Event, EventData, EventLogBackend, LogCursor,
    MemoryStore, Projector, StreamRevision,
};

// ── Fixtures ────────────────────────────────────────────────────────

#[derive(Default, Clone, Serialize, Deserialize)]
struct Ping {
    id: Uuid,
}
impl Event for Ping {
    const NAME: &'static str = "ping";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct OtherFact {
    id: Uuid,
}
impl Event for OtherFact {
    const NAME: &'static str = "other";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

/// Dedup-gate-style aggregate folded from `ping` facts.
#[derive(Default, Clone, Serialize, Deserialize)]
struct PingCount {
    n: u32,
}
impl Aggregate for PingCount {
    const NAME: &'static str = "PingCount";
    const SUBJECT: &'static str = "ping";
}
impl Apply<Ping> for PingCount {
    fn apply(&mut self, _: &Ping) {
        self.n += 1;
    }
}

/// A projector subscribed to a DIFFERENT fact type ("other"), so the
/// poison `ping` event only ever meets the FOLD path — never the
/// projector-body deserialize path. This mirrors the real hazard: a
/// consumer whose registry folds counters/dedup gates from facts it
/// does not itself dispatch on.
struct NoopProjector;

#[async_trait]
impl Projector for NoopProjector {
    type Event = OtherFact;
    const NAME: &'static str = "audit.v7.noop";

    async fn project(&self, _fact: &OtherFact, _ctx: Ctx<'_>) -> Result<()> {
        Ok(())
    }
}

async fn append_ping_payload(store: &MemoryStore, subject: Uuid, payload: serde_json::Value) {
    append_event(
        store,
        EventData {
            event_id: Uuid::new_v4(),
            causation_id: None,
            workflow_id: Uuid::new_v4(),
            event_type: "ping".into(),
            payload,
            created_at: Utc::now(),
            category: Some("ping".into()),
            subject_id: Some(subject),
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        },
    )
    .await
    .unwrap();
}

// ── Test 1: the documented contract — a bad payload is fatal ────────
#[tokio::test]
async fn bad_payload_must_be_fatal_not_silently_excluded() {
    let store = Arc::new(MemoryStore::new());
    let subject = Uuid::new_v4();

    // Poison: does not deserialize as Ping (missing required `id`).
    append_ping_payload(&store, subject, serde_json::json!({ "not": "a ping" })).await;
    // A good ping after it, same stream (revision 1).
    append_ping_payload(&store, subject, serde_json::to_value(Ping { id: subject }).unwrap())
        .await;

    let mut reg = AggregatorRegistry::new();
    reg.register(Aggregator::for_type::<PingCount, Ping>());
    let reg = Arc::new(reg);

    let runner = ProjectionRunner::new(
        NoopProjector,
        "audit.v7.noop",
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn causal::CheckpointStore>,
    )
    .with_aggregators(reg.clone());

    let step_res = tokio::time::timeout(Duration::from_secs(10), runner.step(10))
        .await
        .expect("step must not wedge");

    let all = tokio::time::timeout(
        Duration::from_secs(10),
        EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 50),
    )
    .await
    .expect("read_all must not wedge")
    .unwrap();
    let parked = all
        .iter()
        .filter(|e| e.event_type == PROJECTION_FAILED_KIND)
        .count();

    // Evidence dump.
    let (_, folded) = reg.get_transition::<PingCount>(subject);
    let cursor = causal::CheckpointStore::get(store.as_ref(), "audit.v7.noop")
        .await
        .unwrap();
    println!(
        "step_res.is_err() = {}, parked_facts = {}, PingCount.n = {}, cursor = {:?} \
         (log has 2 ping events; poison is revision 0)",
        step_res.is_err(),
        parked,
        folded.n,
        cursor,
    );

    assert!(
        step_res.is_err() || parked > 0,
        "documented contract: a payload that fails aggregate deserialization is \
         fatal to the fold — the step must error or park a PROJECTION_FAILED fact. \
         Instead the event was silently excluded from PingCount (n = {}) with zero \
         telemetry and the cursor advanced to {:?}",
        folded.n,
        cursor,
    );
}

// ── Test 2: live fold vs restore replay must agree ──────────────────
#[tokio::test]
async fn live_fold_and_restore_replay_agree_on_bad_payload() {
    let mut reg = AggregatorRegistry::new();
    reg.register(Aggregator::for_type::<PingCount, Ping>());

    let subject = Uuid::new_v4();
    let poison = serde_json::json!({ "not": "a ping" });

    // Live fold path (what emit/append, runners, hydration use via fold_event).
    let live = reg.apply_event(
        "ping",
        &poison,
        subject,
        "ping",
        StreamRevision::ZERO,
        LogCursor::from_raw(1),
    );

    // Restore/replay path (what restore_aggregate / state_of uses).
    let mut state = PingCount::default();
    let replay = reg.replay_events_onto("PingCount", &mut state, &[("ping", &poison)]);

    println!(
        "live fold: {:?} (applied would be silent-skip), restore replay: {:?}",
        live.as_ref().map(|o| o.applied),
        replay.as_ref().err().map(|e| e.to_string()),
    );

    assert_eq!(
        live.is_err(),
        replay.is_err(),
        "the same undeserializable payload must be treated the same on the live \
         fold path and the restore/replay path — one silently skipping while the \
         other hard-errors means aggregate state depends on which code path \
         happened to compute it",
    );
}
