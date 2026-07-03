//! Audit verifier #104 — two distinct Rust event types sharing
//! NAME + SUBJECT: does one context's fact permanently wedge the
//! other context's OCC append path for that aggregate id?
//!
//! CORRECT behavior would be either (a) the builder rejects the
//! colliding registration (it has `event_type_id` available), or
//! (b) `append` tolerates the sibling's payload. The claimed defect
//! is that neither happens: the fold's hard `?` cross-deserializes
//! the sibling payload and every later append/read for that id errors.

use std::sync::Arc;
use std::time::Duration;

use causal::aggregate::{Aggregate, Apply};
use causal::{
    Aggregator, CheckpointStore, EngineBuilder, Event, EventLogBackend, MemoryStore,
    ReactorCheckpoint,
};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

const T: Duration = Duration::from_secs(10);

// ── Bounded context A: status is a string ──
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobStatusA {
    job_id: Uuid,
    status: String, // required field; absent from context B's payload
}
impl Event for JobStatusA {
    const NAME: &'static str = "status_changed";
    const SUBJECT: &'static str = "job";
    fn subject_id(&self) -> Uuid {
        self.job_id
    }
}

// ── Bounded context B: status is a numeric code ──
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobStatusB {
    job_id: Uuid,
    code: u32, // incompatible with A's shape
    terminal: bool,
}
impl Event for JobStatusB {
    const NAME: &'static str = "status_changed"; // SAME NAME
    const SUBJECT: &'static str = "job"; // SAME SUBJECT → same physical stream
    fn subject_id(&self) -> Uuid {
        self.job_id
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct LifecycleA {
    transitions: u64,
}
impl Aggregate for LifecycleA {
    const NAME: &'static str = "LifecycleA";
    const SUBJECT: &'static str = "job";
    const INVARIANT: bool = true; // OCC-fenced: append is the only write door
}
impl Apply<JobStatusA> for LifecycleA {
    fn apply(&mut self, _f: &JobStatusA) {
        self.transitions += 1;
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct LifecycleB {
    transitions: u64,
}
impl Aggregate for LifecycleB {
    const NAME: &'static str = "LifecycleB";
    const SUBJECT: &'static str = "job";
    const INVARIANT: bool = true;
}
impl Apply<JobStatusB> for LifecycleB {
    fn apply(&mut self, _f: &JobStatusB) {
        self.transitions += 1;
    }
}

#[tokio::test]
async fn same_name_sibling_type_must_not_wedge_appends() {
    let store = Arc::new(MemoryStore::new());
    // Step 1: the builder must either reject this registration (two
    // DIFFERENT Rust types under one NAME) or make it safe. It has
    // event_type_id on every Aggregator. If it panics here, that is
    // CORRECT behavior and the finding is refuted.
    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators([
        Aggregator::for_type::<LifecycleA, JobStatusA>(),
        Aggregator::for_type::<LifecycleB, JobStatusB>(),
    ])
    .build()
    .await
    .expect("engine build");

    let id = Uuid::new_v4();

    // Step 2: context B writes its fact via the OCC door.
    let first = timeout(
        T,
        engine.append::<LifecycleB, JobStatusB, _>(id, |_s| {
            Ok(vec![JobStatusB { job_id: id, code: 7, terminal: false }])
        }),
    )
    .await
    .expect("append B timed out");
    assert!(first.is_ok(), "context B's own append should work: {:?}", first.err());

    // Step 3: no alternate write door for A — emit is OCC-fenced for
    // this NAME (both aggregates are invariant and share the fence key).
    let emit_res = timeout(T, engine.emit(JobStatusA { job_id: id, status: "ok".into() }))
        .await
        .expect("emit timed out");
    println!("emit(JobStatusA) => {:?}", emit_res.as_ref().err().map(|e| format!("{e:#}")));

    // Step 4 (the core claim): context A's append against the same id.
    // CORRECT behavior: succeeds (folds only A's own facts). DEFECT:
    // hard-errors deserializing B's payload — permanently.
    let second = timeout(
        T,
        engine.append::<LifecycleA, JobStatusA, _>(id, |_s| {
            Ok(vec![JobStatusA { job_id: id, status: "started".into() }])
        }),
    )
    .await
    .expect("append A timed out");
    let second_err = second.as_ref().err().map(|e| format!("{e:#}"));
    println!("append<LifecycleA, JobStatusA> attempt 1 => {second_err:?}");

    // Step 5: permanence — retry does not clear it (not transient).
    let third = timeout(
        T,
        engine.append::<LifecycleA, JobStatusA, _>(id, |_s| {
            Ok(vec![JobStatusA { job_id: id, status: "started".into() }])
        }),
    )
    .await
    .expect("append A retry timed out");
    println!(
        "append<LifecycleA, JobStatusA> attempt 2 => {:?}",
        third.as_ref().err().map(|e| format!("{e:#}"))
    );

    // Step 6: read side — load::<LifecycleA, JobStatusA> hits the same fold.
    let loaded = timeout(T, engine.load::<LifecycleA, JobStatusA>(id))
        .await
        .expect("load timed out");
    println!(
        "load<LifecycleA, JobStatusA> => {:?}",
        loaded.as_ref().err().map(|e| format!("{e:#}"))
    );

    assert!(
        second.is_ok() && third.is_ok(),
        "aggregate id {id} is WEDGED for context A by context B's same-NAME \
         fact: append errors permanently (attempt1: {:?}, attempt2: {:?}) and \
         emit is OCC-fenced ({:?}) so there is no write door at all",
        second_err,
        third.as_ref().err().map(|e| format!("{e:#}")),
        emit_res.as_ref().err().map(|e| format!("{e:#}")),
    );
}
