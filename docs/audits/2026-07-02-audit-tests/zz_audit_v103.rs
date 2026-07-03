//! Audit verifier test for finding #103.
//!
//! Claim: `EngineBuilder::build()` never validates `A::SUBJECT ==
//! F::SUBJECT` for restorable aggregates, so a one-string mismatch
//! (aggregate declares SUBJECT = "account", event leaves SUBJECT at its
//! default = NAME) builds cleanly. Appends then land durably on
//! `{F::SUBJECT}-{id}` while every fold hits the runtime stream-
//! alignment bail (aggregator.rs `apply_event`), which the emit path
//! swallows as a warn — so `state_of` returns `Ok(None)` forever
//! despite the aggregate having durable events.
//!
//! CORRECT behavior passes this test via either:
//!   1. build() rejecting the statically-detectable misconfiguration, or
//!   2. state_of() reflecting the durably appended fact.
//! The claimed defect makes build() succeed AND state_of() return None
//! -> the final assertion fails.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::{
    CheckpointStore, EngineBuilder, Event, EventLogBackend, MemoryStore,
    ReactorCheckpoint, SnapshotStore,
};

#[derive(Clone, Serialize, Deserialize)]
struct Deposited {
    account: Uuid,
    amount: i64,
}
impl Event for Deposited {
    const NAME: &'static str = "deposit";
    // SUBJECT deliberately left at its default (= NAME = "deposit")
    // while the aggregate below declares SUBJECT = "account" — the
    // one-string mismatch from the finding (forgot `subject = "account"`).
    fn subject_id(&self) -> Uuid {
        self.account
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Balance {
    value: i64,
}
impl Aggregate for Balance {
    const NAME: &'static str = "Balance";
    const SUBJECT: &'static str = "account";
}
impl Apply<Deposited> for Balance {
    fn apply(&mut self, e: &Deposited) {
        self.value += e.amount;
    }
}

const T: Duration = Duration::from_secs(10);

#[tokio::test]
async fn subject_mismatch_is_rejected_at_build_or_still_folds() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let acct = Uuid::new_v4();

    let built = timeout(
        T,
        EngineBuilder::new(
            mem.clone() as Arc<dyn EventLogBackend>,
            mem.clone() as Arc<dyn CheckpointStore>,
            mem.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![Aggregator::for_type::<Balance, Deposited>()])
        .with_snapshot_store(mem.clone() as Arc<dyn SnapshotStore>)
        .build(),
    )
    .await?;

    // CORRECT option 1: build() rejects the misconfiguration loudly.
    let engine = match built {
        Err(e) => {
            eprintln!("build() rejected the SUBJECT mismatch (correct): {e:#}");
            return Ok(());
        }
        Ok(e) => e,
    };
    eprintln!("build() ACCEPTED the A::SUBJECT != F::SUBJECT wiring");

    // The engine accepted the wiring — emit must then uphold the promise
    // that a committed fact folds into registered aggregate state.
    let emit_res = timeout(T, async {
        engine.emit(Deposited { account: acct, amount: 100 }).await
    })
    .await?;
    eprintln!("emit result: {:?}", emit_res.as_ref().map(|_| "ok"));
    emit_res?;

    // The append IS durable, on the fact's placement stream {F::SUBJECT}-{id}.
    let placed = timeout(T, mem.read_stream("deposit", acct, None)).await??;
    assert_eq!(
        placed.len(),
        1,
        "durable append must exist on deposit-{acct}"
    );

    // ...while the aggregate's declared restore stream {A::SUBJECT}-{id}
    // is empty (nothing ever wrote there).
    let restore_stream = timeout(T, mem.read_stream("account", acct, None)).await??;
    assert!(
        restore_stream.is_empty(),
        "nothing writes to the aggregate's declared stream account-{acct}"
    );

    // CORRECT option 2: state_of reflects the durably appended fact.
    let state = timeout(T, engine.state_of::<Balance>(acct)).await??;
    assert_eq!(
        state,
        Some(Balance { value: 100 }),
        "DEFECT: emit() committed the fact durably but state_of() sees \
         nothing — the SUBJECT mismatch shipped cleanly through build(), \
         every fold hits the runtime alignment bail (swallowed as a warn), \
         and restore reads the empty account-{{id}} stream"
    );

    timeout(T, engine.shutdown()).await??;
    Ok(())
}

// Same mismatch, restart shape: a second engine over the same durable
// store restores nothing, so reads report "no aggregate" forever even
// after a clean restart.
#[tokio::test]
async fn subject_mismatch_restart_restores_nothing() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let acct = Uuid::new_v4();

    let build = |mem: &Arc<MemoryStore>| {
        EngineBuilder::new(
            mem.clone() as Arc<dyn EventLogBackend>,
            mem.clone() as Arc<dyn CheckpointStore>,
            mem.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![Aggregator::for_type::<Balance, Deposited>()])
        .with_snapshot_store(mem.clone() as Arc<dyn SnapshotStore>)
        .build()
    };

    let engine = match timeout(T, build(&mem)).await? {
        Err(_) => return Ok(()), // build-time rejection = correct
        Ok(e) => e,
    };
    timeout(T, async {
        engine.emit(Deposited { account: acct, amount: 42 }).await
    })
    .await??;
    timeout(T, engine.shutdown()).await??;

    // Fresh engine, same durable store: restore must recover the state.
    let engine2 = match timeout(T, build(&mem)).await? {
        Err(_) => return Ok(()),
        Ok(e) => e,
    };
    let restored = timeout(T, engine2.state_of::<Balance>(acct)).await??;
    assert_eq!(
        restored,
        Some(Balance { value: 42 }),
        "DEFECT: durable event exists on deposit-{acct} but a restarted \
         engine restores from the empty account-{acct} stream and reports \
         no aggregate"
    );
    timeout(T, engine2.shutdown()).await??;
    Ok(())
}
