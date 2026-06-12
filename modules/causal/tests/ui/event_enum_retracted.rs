// The enum fact form was retracted (no-lying-defaults §1): a family
// enum's trigger deserializes by serde tag, so adding a variant poisons
// every deployed consumer of the family — vocabulary growth as a
// breaking change. One fact = one struct; `subject = "<kind>"` shares a
// history. The macro must say all of that, not just "no".
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(prefix = "job", subject_id = "job_id")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JobEvent {
    Opened { job_id: Uuid },
    Enriched { job_id: Uuid },
}

fn main() {}
