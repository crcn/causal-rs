// Genuinely streamless facts (telemetry, ops counters) opt in to the
// shared `{category}-nil` stream EXPLICITLY — the cost (no per-stream
// aggregate can fold them) is visible at the definition site.
use causal::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(prefix = "telemetry", no_subject)]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TelemetryEvent {
    TickRecorded { n: u64 },
}

#[causal::event(no_subject)]
#[derive(Clone, Serialize, Deserialize)]
struct HeartbeatSeen {
    n: u64,
}

fn main() {
    assert_eq!(
        TelemetryEvent::TickRecorded { n: 1 }.subject_id(),
        Uuid::nil(),
    );
    assert_eq!(HeartbeatSeen { n: 1 }.subject_id(), Uuid::nil());
}
