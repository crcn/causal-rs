// Workflow ROOT (0.10 §3): the fact's workflow is named BY its own
// payload field — constitutionally, at every emit site, from any
// emitter. Chain members (no declaration) inherit their emitter's.
use causal::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(name = "job_opened", subject_id = "job_id", subject = "job",
                workflow_id = "job_id")]
#[derive(Clone, Serialize, Deserialize)]
struct JobOpened {
    job_id: Uuid,
    file_id: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

// Chain member: no declaration — declared_workflow_id() is None.
#[causal::event(name = "signal_corroborated", subject_id = "signal_id")]
#[derive(Clone, Serialize, Deserialize)]
struct SignalCorroborated {
    signal_id: Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

fn main() {
    let job = Uuid::new_v4();
    let f = JobOpened { job_id: job, file_id: Uuid::new_v4(), occurred_at: chrono::Utc::now() };
    assert_eq!(f.declared_workflow_id(), Some(job), "root: workflow FROM the payload");
    assert_eq!(f.subject_id(), job);

    let m = SignalCorroborated { signal_id: Uuid::new_v4(), occurred_at: chrono::Utc::now() };
    assert_eq!(m.declared_workflow_id(), None, "member: inherits the emitter's workflow");
}
