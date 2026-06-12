// Facts without a timestamp field are legitimate: occurred_at() falls
// back to the trait default (None) instead of demanding a field just
// to satisfy the macro. This is the anti-stair-step guarantee — fixing
// the subject_id error must not immediately raise an occurred_at error.
use causal::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(subject_id = "order_id")]
#[derive(Clone, Serialize, Deserialize)]
struct OrderTagged {
    order_id: Uuid,
}

#[causal::event(prefix = "order", subject_id = "order_id")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OrderEvent {
    Tagged { order_id: Uuid },
    Flagged { order_id: Uuid },
}

fn main() {
    let id = Uuid::new_v4();
    let s = OrderTagged { order_id: id };
    assert_eq!(s.subject_id(), id);
    assert_eq!(s.occurred_at(), None);
    let e = OrderEvent::Tagged { order_id: id };
    assert_eq!(e.subject_id(), id);
    assert_eq!(e.occurred_at(), None);
}
