// A typo'd subject_id field name must be a teaching error at the macro,
// not a raw "no field" rustc error pointing into generated code.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(name = "order_placed", subject_id = "oder_id")]
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
}

fn main() {}
