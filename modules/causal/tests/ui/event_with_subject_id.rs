// The normal shape: the fact names the Uuid field it is about.
use causal::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(name = "deposited", subject_id = "account")]
#[derive(Clone, Serialize, Deserialize)]
struct Deposited {
    account: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

// Two fact families sharing ONE subject history — the anti-god-enum
// valve. Both land in `ledger-{account}`; each keeps its own type.
#[causal::event(name = "withdrawn", subject_id = "account", subject = "ledger")]
#[derive(Clone, Serialize, Deserialize)]
struct Withdrawn {
    account: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

fn main() {
    let id = Uuid::new_v4();
    let d = Deposited { account: id, occurred_at: chrono::Utc::now() };
    assert_eq!(d.subject_id(), id);
    assert_eq!(<Deposited as Event>::SUBJECT, "deposited"); // defaults to NAME
    let w = Withdrawn { account: id, occurred_at: chrono::Utc::now() };
    assert_eq!(w.subject_id(), id);
    assert_eq!(<Withdrawn as Event>::SUBJECT, "ledger");  // co-located
}
