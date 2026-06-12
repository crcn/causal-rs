// A no_subject fact with a timestamp field must NOT silently lose it —
// occurred_at() generation follows the same presence rule as the
// subject_id shape.
use causal::Event;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[causal::event(prefix = "sweep", no_subject)]
#[derive(Clone, Serialize, Deserialize)]
struct SweepCompleted {
    occurred_at: DateTime<Utc>,
}

fn main() {
    let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    assert_eq!(SweepCompleted { occurred_at: t }.occurred_at(), Some(t));
}
