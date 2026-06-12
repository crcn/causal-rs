// Reference-carrying subject-less facts opt in EXPLICITLY — the ids
// present are references, not subjects, and the declaration says so.
use causal::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(name = "cache_purged", no_subject)]
#[derive(Clone, Serialize, Deserialize)]
struct CachePurged {
    requested_by: Uuid,   // a reference — without no_subject this is a
    n_evicted: u64,       // teaching error naming `requested_by`
}

fn main() {
    assert_eq!(
        CachePurged { requested_by: Uuid::new_v4(), n_evicted: 3 }.subject_id(),
        Uuid::nil(),
    );
}
