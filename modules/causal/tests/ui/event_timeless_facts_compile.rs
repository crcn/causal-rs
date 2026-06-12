// Facts without a timestamp field are legitimate: occurred_at() falls
// back to the trait default (None) instead of demanding a field just
// to satisfy the macro — fixing the subject error never raises a
// second error (the anti-stair-step guarantee).
use causal::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(subject_id = "order_id")]
#[derive(Clone, Serialize, Deserialize)]
struct OrderTagged {
    order_id: Uuid,
}

fn main() {
    let id = Uuid::new_v4();
    let s = OrderTagged { order_id: id };
    assert_eq!(s.subject_id(), id);
    assert_eq!(s.occurred_at(), None);
}
