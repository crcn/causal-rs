// `stream_id` and `nil_stream` contradict each other — pick one.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(prefix = "order", stream_id = "order_id", nil_stream)]
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

fn main() {}
