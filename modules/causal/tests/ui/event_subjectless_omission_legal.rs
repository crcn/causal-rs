// Shape-gated omission: a fact with NO scalar Uuid fields cannot name a
// subject, so omitting the declaration is unambiguous and legal — no
// `no_subject` ceremony for trivially subject-less facts. The moment
// this struct gains a Uuid field it becomes a teaching error asking
// whether that field is its subject.
use causal::Event;
use uuid::Uuid;

#[causal::event(prefix = "telemetry", ephemeral)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TickRecorded {
    n: u64,
}

fn main() {
    assert_eq!(TickRecorded { n: 1 }.subject_id(), Uuid::nil());
}
