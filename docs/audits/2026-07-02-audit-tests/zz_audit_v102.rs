//! Audit verification for finding #102: OCC fence keyed by event NAME
//! while stream placement is keyed by SUBJECT.
//!
//! Fixture: `OrderAgg` is an INVARIANT aggregate over `OrderPlaced`
//! (NAME = SUBJECT = "order_placed"). `AuditNote` is a foreign event
//! kind (NAME = "audit_note") that declares SUBJECT = "order_placed",
//! co-locating into the invariant aggregate's physical stream.
//!
//! Test 1: the emit fence (engine.rs:2420 checks fact.name()) does not
//! consider SUBJECT, so the AuditNote Any-appends into the OCC stream.
//! Test 2: the consequence — a lone OCC writer with ZERO genuine
//! contention on its own fact kind exhausts MAX_OCC_RETRIES against
//! sustained co-located Any-append traffic and returns ConflictError.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::{Aggregator, Engine, EngineBuilder};

// ── Fixtures ─────────────────────────────────────────────────────────

/// The invariant aggregate's fact kind. NAME = SUBJECT = "order_placed".
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    occurred_at: DateTime<Utc>,
}
impl Event for OrderPlaced {
    const NAME: &'static str = "order_placed";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
    fn occurred_at(&self) -> Option<DateTime<Utc>> {
        Some(self.occurred_at)
    }
}

/// Foreign fact kind co-locating into the invariant stream: different
/// NAME, same SUBJECT. This is the documented co-location feature
/// (`Event::SUBJECT` doc: "a fact streams by its own kind unless it
/// co-locates with other fact families in one subject history").
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditNote {
    order_id: Uuid,
    note: String,
}
impl Event for AuditNote {
    const NAME: &'static str = "audit_note";
    const SUBJECT: &'static str = "order_placed"; // co-locate with orders
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct OrderAgg {
    placed: u32,
}
impl Aggregate for OrderAgg {
    const NAME: &'static str = "OrderAgg";
    const INVARIANT: bool = true; // installs the OCC fence for "order_placed"
}
impl Apply<OrderPlaced> for OrderAgg {
    fn apply(&mut self, _f: &OrderPlaced) {
        self.placed += 1;
    }
}

async fn engine(store: &Arc<MemoryStore>) -> Engine {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators([Aggregator::for_type::<OrderAgg, OrderPlaced>()])
    .build()
    .await
    .unwrap()
}

// ── Test 1: the fence hole itself (deterministic) ────────────────────
//
// The INVARIANT contract (aggregate.rs): "the only write path is the
// OCC command door Engine::append"; C11 (aggregate.rs:10): "Reactors
// cannot emit into Aggregate streams". If the fence were stream-level,
// the subject-colliding emit below would be rejected, or at minimum
// would not land in the OCC-protected stream. Failure here proves the
// hole: fence checks NAME, placement uses SUBJECT.
#[tokio::test]
async fn subject_colliding_emit_is_fenced_out_of_invariant_stream() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store).await;
    let order = Uuid::new_v4();

    // Sanity: the fence DOES reject the fenced NAME.
    let err = engine
        .emit(OrderPlaced { order_id: order, occurred_at: Utc::now() })
        .await
        .expect_err("emit of the invariant fact kind must be rejected");
    assert!(format!("{err:?}").contains("OCC-required"), "got: {err:?}");

    // The subject-colliding foreign kind: same stream, different NAME.
    let res = timeout(
        Duration::from_secs(10),
        engine.emit(AuditNote { order_id: order, note: "looks fine".into() }),
    )
    .await
    .expect("emit must not hang");

    // Correct behavior (stream-level fence): either the emit errors, or
    // the event does not land in the invariant aggregate's stream.
    let in_stream = EventLogBackend::read_stream(store.as_ref(), "order_placed", order, None)
        .await
        .unwrap();
    let landed = in_stream.iter().any(|e| e.event_type == "audit_note");
    assert!(
        res.is_err() || !landed,
        "DEFECT: un-fenced Any-append landed in the INVARIANT aggregate's \
         stream order_placed-{order} via emit (fence keys on NAME, placement \
         on SUBJECT); stream now holds {} events",
        in_stream.len(),
    );
    engine.shutdown().await.unwrap();
}

// ── Test 2: the liveness consequence ─────────────────────────────────
//
// A single OCC writer — the ONLY writer of "order_placed" facts, i.e.
// zero genuine contention on the fact kind the caller opted into OCC
// for — runs against sustained co-located AuditNote emits. Correct
// behavior: the append succeeds (foreign traffic is either fenced out
// or does not consume the caller's bounded retry budget). Defect: every
// attempt's read→CAS window is bumped by an Any-append, all 16 retries
// conflict, and append returns an error with no genuine contention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lone_occ_writer_survives_colocated_any_append_traffic() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(engine(&store).await);
    let order = Uuid::new_v4();

    // Foreign-traffic task: continuously emit AuditNotes into the same
    // subject history. (If the fence were subject-aware these would all
    // be rejected and the loop just spins harmlessly.)
    let stop = Arc::new(AtomicBool::new(false));
    let foreign_appends = Arc::new(AtomicUsize::new(0));
    let emitter = {
        let engine = engine.clone();
        let stop = stop.clone();
        let n = foreign_appends.clone();
        tokio::spawn(async move {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                i += 1;
                if engine
                    .emit(AuditNote { order_id: order, note: format!("note {i}") })
                    .await
                    .is_ok()
                {
                    n.fetch_add(1, Ordering::Relaxed);
                }
                tokio::task::yield_now().await;
            }
        })
    };

    // Give the emitter a head start so traffic is flowing.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The lone OCC writer. The decide body sleeps 2ms to model a real
    // decision (fold + business logic); this widens the read→CAS window
    // each attempt, which sustained foreign traffic then always bumps.
    let decide_runs = Arc::new(AtomicUsize::new(0));
    let dr = decide_runs.clone();
    let result = timeout(
        Duration::from_secs(30),
        engine.append::<OrderAgg, OrderPlaced, _>(order, move |_state| {
            dr.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(2));
            Ok(vec![OrderPlaced { order_id: order, occurred_at: Utc::now() }])
        }),
    )
    .await
    .expect("append must not hang");

    stop.store(true, Ordering::Relaxed);
    let _ = emitter.await;

    let foreign = foreign_appends.load(Ordering::Relaxed);
    let attempts = decide_runs.load(Ordering::Relaxed);
    assert!(
        result.is_ok(),
        "DEFECT: the ONLY writer of 'order_placed' facts exhausted its OCC \
         retry budget ({attempts} decide attempts) purely on co-located \
         Any-append traffic ({foreign} foreign audit_note emits landed in the \
         invariant stream) — spurious ConflictError with zero genuine \
         contention: {:?}",
        result.err(),
    );

    Arc::try_unwrap(engine)
        .unwrap_or_else(|_| panic!("engine still shared"))
        .shutdown()
        .await
        .unwrap();
}
