// `ephemeral` was parsed but never had any effect (a "lying default").
// It is now a teaching error: subject-less-ness is inferred from shape
// (no scalar Uuid fields), so the keyword is redundant and rejected.
use uuid::Uuid;

#[causal::event(name = "tick_recorded", ephemeral)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TickRecorded {
    n: u64,
}

fn main() {
    let _ = Uuid::nil();
}
