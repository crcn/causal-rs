// The struct form has the same trap as the enum form: no `stream_id`
// silently meant `Uuid::nil()`. Must not compile without the explicit
// `nil_stream` opt-in.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event]
#[derive(Clone, Serialize, Deserialize)]
struct EnrichmentReady {
    correlation_id: Uuid,
}

fn main() {}
