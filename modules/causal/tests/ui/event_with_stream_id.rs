// The normal shape: the event names the Uuid field it streams by.
use causal::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(prefix = "deposit", stream_id = "account")]
#[derive(Clone, Serialize, Deserialize)]
struct Deposited {
    account: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

#[causal::event(prefix = "curiosity", stream_id = "signal_id")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CuriosityEvent {
    SignalInvestigated {
        signal_id: Uuid,
        occurred_at: chrono::DateTime<chrono::Utc>,
    },
}

fn main() {
    let id = Uuid::new_v4();
    let d = Deposited { account: id, occurred_at: chrono::Utc::now() };
    assert_eq!(d.stream_id(), id);
    let c = CuriosityEvent::SignalInvestigated {
        signal_id: id,
        occurred_at: chrono::Utc::now(),
    };
    assert_eq!(c.stream_id(), id);
}
