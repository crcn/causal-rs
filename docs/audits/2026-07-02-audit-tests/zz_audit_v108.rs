//! AUDIT VERIFIER #108 — Uuid type alias defeats the #[event] subject-shape gate.
//!
//! The shape gate (require_subject_identity / candidate_subject_fields in
//! causal_core_macros) is supposed to make it impossible to compile a fact
//! that carries a scalar Uuid id without declaring `subject_id` or
//! `no_subject`. The check is syntactic (last path segment == "Uuid"), so a
//! domain alias `type OrderId = Uuid` is invisible: the fact compiles as
//! "provably subject-less" and the macro generates
//! `subject_id() -> Uuid::nil()` — the exact pre-0.9 nil fan-in the gate
//! exists to prevent.
//!
//! CORRECT behavior: this file should not compile at all (teaching error),
//! or, at minimum, distinct orders must occupy distinct subjects. The
//! assertions below encode the correct behavior, so they FAIL if the defect
//! is real.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::{CheckpointStore, Engine, EngineBuilder, Event, EventLogBackend, MemoryStore, ReactorCheckpoint};

// The trap: a perfectly idiomatic domain alias.
type OrderId = Uuid;

// NOTE: no `subject_id`, no `no_subject`. With `order_id: Uuid` this is a
// compile error (see tests/ui/event_struct_missing_subject_id.rs). With the
// alias it compiles silently.
#[causal::event(name = "order_placed")]
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: OrderId,
    amount_cents: u64,
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct OrderTotal {
    placed: u64,
    cents: u64,
}
impl Aggregate for OrderTotal {
    const NAME: &'static str = "OrderTotal";
}
impl Apply<OrderPlaced> for OrderTotal {
    fn apply(&mut self, e: &OrderPlaced) {
        self.placed += 1;
        self.cents += e.amount_cents;
    }
}

async fn build(mem: &Arc<MemoryStore>) -> Engine {
    EngineBuilder::new(
        mem.clone() as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators(vec![Aggregator::for_type::<OrderTotal, OrderPlaced>()])
    .build()
    .await
    .unwrap()
}

// Level 1: the macro output itself. Two different orders must not share a
// subject, and no order's subject may be the nil fan-in stream.
#[test]
fn alias_typed_id_field_must_not_collapse_subjects_to_nil() {
    let a = OrderPlaced { order_id: Uuid::new_v4(), amount_cents: 100 };
    let b = OrderPlaced { order_id: Uuid::new_v4(), amount_cents: 250 };

    assert_ne!(
        a.subject_id(),
        Uuid::nil(),
        "DEFECT: alias-typed id field silently produced the nil subject \
         (shape gate bypassed; pre-0.9 fan-in re-created)"
    );
    assert_ne!(
        a.subject_id(),
        b.subject_id(),
        "DEFECT: two distinct orders share one subject stream"
    );
}

// Level 2: downstream corruption. The default aggregator keys by
// Event::subject_id, so every order folds into the single key
// "OrderTotal:00000000-0000-0000-0000-000000000000" and per-order reads
// return nothing.
#[tokio::test]
async fn alias_typed_id_field_must_not_fan_in_all_orders_into_one_aggregate() {
    let mem = Arc::new(MemoryStore::new());
    let engine = build(&mem).await;

    let order_a = Uuid::new_v4();
    let order_b = Uuid::new_v4();

    timeout(Duration::from_secs(10), async {
        engine
            .emit(OrderPlaced { order_id: order_a, amount_cents: 100 })
            .await
            .unwrap();
        engine
            .emit(OrderPlaced { order_id: order_b, amount_cents: 250 })
            .await
            .unwrap();
    })
    .await
    .expect("emits wedged");

    let state_a = timeout(Duration::from_secs(10), engine.state_of::<OrderTotal>(order_a))
        .await
        .expect("state_of wedged")
        .unwrap();
    let state_nil = timeout(Duration::from_secs(10), engine.state_of::<OrderTotal>(Uuid::nil()))
        .await
        .expect("state_of wedged")
        .unwrap();

    eprintln!("state_of(order_a) = {state_a:?}");
    eprintln!("state_of(nil)     = {state_nil:?}");

    // Correct behavior: order A's aggregate exists under order A's id and
    // contains exactly order A's data; nothing accumulates under nil.
    assert_eq!(
        state_a,
        Some(OrderTotal { placed: 1, cents: 100 }),
        "DEFECT: per-order aggregate read at the real order id is wrong \
         (events were folded elsewhere)"
    );
    assert_eq!(
        state_nil, None,
        "DEFECT: all orders folded into the single nil-keyed aggregate"
    );

    timeout(Duration::from_secs(10), engine.shutdown())
        .await
        .expect("shutdown wedged")
        .unwrap();
}
