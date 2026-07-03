//! Audit verifier #101: does `Engine::append::<A, F>`'s decide closure see
//! the FULL multi-type aggregate state for a documented co-located
//! restorable aggregate, or only the F-typed partial fold?
//!
//! Fixtures mirror modules/causal/tests/aggregate_restore_test.rs (the
//! blessed multi-event co-located layout: Apply<Deposited> + Apply<Withdrawn>
//! on one "account" stream), plus INVARIANT = true so `append` is the only
//! write door (emit fenced), exactly the configuration the finding names.
//!
//! Correct behavior (invariants enforced against the aggregate's real
//! state) makes these tests PASS. The claimed defect (F-only fold hands
//! decide partial state) makes them FAIL.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::{
    CheckpointStore, Engine, EngineBuilder, Event, EventLogBackend, MemoryStore,
    ReactorCheckpoint,
};

const T: Duration = Duration::from_secs(20);

// ── Three DISTINCT event types co-located in ONE "account" stream ──────

#[derive(Clone, Serialize, Deserialize)]
struct Deposited {
    account: Uuid,
    amount: i64,
}
impl Event for Deposited {
    const NAME: &'static str = "deposit";
    const SUBJECT: &'static str = "account";
    fn subject_id(&self) -> Uuid {
        self.account
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Withdrawn {
    account: Uuid,
    amount: i64,
}
impl Event for Withdrawn {
    const NAME: &'static str = "withdraw";
    const SUBJECT: &'static str = "account";
    fn subject_id(&self) -> Uuid {
        self.account
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Frozen {
    account: Uuid,
}
impl Event for Frozen {
    const NAME: &'static str = "frozen";
    const SUBJECT: &'static str = "account";
    fn subject_id(&self) -> Uuid {
        self.account
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Balance {
    value: i64,
    frozen: bool,
}
impl Aggregate for Balance {
    const NAME: &'static str = "Balance";
    const SUBJECT: &'static str = "account";
    // The invariant-carrying case: emit rejects these fact kinds,
    // `Engine::append` is the only write door (aggregate.rs INVARIANT docs).
    const INVARIANT: bool = true;
}
impl Apply<Deposited> for Balance {
    fn apply(&mut self, e: &Deposited) {
        self.value += e.amount;
    }
}
impl Apply<Withdrawn> for Balance {
    fn apply(&mut self, e: &Withdrawn) {
        self.value -= e.amount;
    }
}
impl Apply<Frozen> for Balance {
    fn apply(&mut self, _: &Frozen) {
        self.frozen = true;
    }
}

async fn build(mem: &Arc<MemoryStore>) -> Engine {
    EngineBuilder::new(
        mem.clone() as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators(vec![
        Aggregator::for_type::<Balance, Deposited>(),
        Aggregator::for_type::<Balance, Withdrawn>(),
        Aggregator::for_type::<Balance, Frozen>(),
    ])
    .build()
    .await
    .unwrap()
}

/// Prong 1 — wrongly REJECTED: a withdrawal guarded by `balance >= amount`
/// is refused because decide never sees the co-located Deposited events,
/// while `engine.state_of::<Balance>` simultaneously reports the funds.
#[tokio::test]
async fn append_decide_sees_full_state_sufficient_funds_withdrawal_accepted() {
    let mem = Arc::new(MemoryStore::new());
    let engine = build(&mem).await;
    let account = Uuid::new_v4();

    // Fund the account through the only permitted write door.
    timeout(
        T,
        engine.append::<Balance, Deposited, _>(account, move |_| {
            Ok(vec![Deposited { account, amount: 100 }])
        }),
    )
    .await
    .expect("deposit timed out")
    .expect("deposit append failed");

    // What balance did the invariant guard actually observe?
    let observed: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let observed_c = observed.clone();

    let res = timeout(
        T,
        engine.append::<Balance, Withdrawn, _>(account, move |b: &Balance| {
            *observed_c.lock().unwrap() = Some(b.value);
            if b.value >= 60 {
                Ok(vec![Withdrawn { account, amount: 60 }])
            } else {
                anyhow::bail!("insufficient funds: decide observed balance {}", b.value)
            }
        }),
    )
    .await
    .expect("withdraw timed out");

    let seen = observed.lock().unwrap().expect("decide never ran");

    // Read-side truth for the SAME aggregate + id.
    let state = timeout(T, engine.state_of::<Balance>(account))
        .await
        .expect("state_of timed out")
        .expect("state_of failed")
        .expect("state_of returned None");

    println!(
        "decide observed balance = {seen}; state_of reports balance = {}",
        state.value
    );

    assert_eq!(
        seen, state.value,
        "decide and state_of must agree on one aggregate's state \
         (decide saw {seen}, state_of saw {})",
        state.value
    );
    assert!(
        res.is_ok(),
        "withdrawal of 60 against a balance of 100 must be accepted; got: {:?}",
        res.err()
    );

    engine.shutdown().await.unwrap();
}

/// Prong 2 — wrongly ACCEPTED: a withdrawal guarded by `!frozen` lands
/// durably on a frozen account, because decide never sees the co-located
/// Frozen event. This durably writes an invariant-violating fact.
#[tokio::test]
async fn append_decide_sees_frozen_flag_withdrawal_on_frozen_account_rejected() {
    let mem = Arc::new(MemoryStore::new());
    let engine = build(&mem).await;
    let account = Uuid::new_v4();

    timeout(
        T,
        engine.append::<Balance, Deposited, _>(account, move |_| {
            Ok(vec![Deposited { account, amount: 100 }])
        }),
    )
    .await
    .expect("deposit timed out")
    .expect("deposit append failed");

    timeout(
        T,
        engine.append::<Balance, Frozen, _>(account, move |_| {
            Ok(vec![Frozen { account }])
        }),
    )
    .await
    .expect("freeze timed out")
    .expect("freeze append failed");

    // Sanity: the read side agrees the account is frozen.
    let state = timeout(T, engine.state_of::<Balance>(account))
        .await
        .expect("state_of timed out")
        .expect("state_of failed")
        .expect("state_of returned None");
    assert!(state.frozen, "read side must see the Frozen fact");

    let res = timeout(
        T,
        engine.append::<Balance, Withdrawn, _>(account, move |b: &Balance| {
            if b.frozen {
                anyhow::bail!("account frozen — withdrawal refused")
            }
            Ok(vec![Withdrawn { account, amount: 60 }])
        }),
    )
    .await
    .expect("withdraw timed out");

    // Durable outcome: did an invariant-violating Withdrawn land?
    let stream = EventLogBackend::read_stream(mem.as_ref(), "account", account, None)
        .await
        .unwrap();
    let withdrawals: Vec<_> = stream
        .iter()
        .filter(|e| e.event_type.as_str() == "withdraw")
        .collect();

    println!(
        "append on frozen account returned Ok = {}; durable withdraw events = {}",
        res.is_ok(),
        withdrawals.len()
    );

    assert!(
        res.is_err(),
        "withdrawal on a frozen account must be refused by the invariant guard"
    );
    assert!(
        withdrawals.is_empty(),
        "no Withdrawn fact may be durably appended to a frozen account; found {}",
        withdrawals.len()
    );

    engine.shutdown().await.unwrap();
}
