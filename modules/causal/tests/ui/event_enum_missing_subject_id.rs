// Omitting `subject_id` (without the explicit `no_subject` opt-in) must
// not compile: the old silent nil-stream default routed every variant
// of every entity into one `{prefix}-nil` stream — unreadable by
// per-stream aggregate folds, invisible until the read fails.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(prefix = "curiosity")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CuriosityEvent {
    SignalInvestigated { signal_id: Uuid },
}

fn main() {}
