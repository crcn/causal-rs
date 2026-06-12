// A no_subject event with timestamp fields must NOT silently lose them
// — occurred_at() generation follows the same presence rule as the
// subject_id shape. Unit variants (which cannot carry fields) get honest
// per-variant None.
use causal::Event;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[causal::event(prefix = "telemetry", no_subject)]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TelemetryEvent {
    TickRecorded { n: u64, occurred_at: DateTime<Utc> },
    Heartbeat,
}

#[causal::event(no_subject)]
#[derive(Clone, Serialize, Deserialize)]
struct SweepCompleted {
    occurred_at: DateTime<Utc>,
}

fn main() {
    let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    assert_eq!(
        TelemetryEvent::TickRecorded { n: 1, occurred_at: t }.occurred_at(),
        Some(t),
        "named variant keeps its timestamp",
    );
    assert_eq!(
        TelemetryEvent::Heartbeat.occurred_at(),
        None,
        "fieldless variant is honestly None",
    );
    assert_eq!(SweepCompleted { occurred_at: t }.occurred_at(), Some(t));
}
