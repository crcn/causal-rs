// `subject_id` and `no_subject` contradict each other — pick one.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(prefix = "order", subject_id = "order_id", no_subject)]
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

fn main() {}
