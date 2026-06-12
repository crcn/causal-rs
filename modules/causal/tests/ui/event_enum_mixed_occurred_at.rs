// Mixed occurred_at presence across variants is almost always a typo
// on the odd variant out — silently returning None for just that
// variant would be a data bug. Must error naming the variant.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(prefix = "order", subject_id = "order_id")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OrderEvent {
    Placed {
        order_id: Uuid,
        occurred_at: chrono::DateTime<chrono::Utc>,
    },
    Shipped {
        order_id: Uuid,
        // occurred_at forgotten here
    },
}

fn main() {}
