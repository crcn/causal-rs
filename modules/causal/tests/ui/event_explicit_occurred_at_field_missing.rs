// An explicit occurred_at_field states intent — a missing field is a
// teaching error, never silently ignored.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(stream_id = "order_id", occurred_at_field = "happened_at")]
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
}

fn main() {}
