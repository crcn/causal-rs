// `workflow_id = "..."` must name a real field — the workflow is
// stamped FROM the payload, so a typo can't silently fall back.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(name = "job_opened", subject_id = "job_id", workflow_id = "jobid")]
#[derive(Clone, Serialize, Deserialize)]
struct JobOpened {
    job_id: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

fn main() {}
