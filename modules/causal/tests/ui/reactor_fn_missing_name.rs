// `name` is REQUIRED on #[reactor]: it names a DURABLE CURSOR. A
// fn-name-derived default would turn a rename refactor into an orphaned
// cursor (backlog silently skipped at reseed).
use anyhow::Result;
use causal::contexts::Ctx;
use causal::reactor::Events;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(name = "n_job_opened", subject_id = "job_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOpened { pub job_id: Uuid }

#[causal::reactors]
mod bad {
    use super::*;

    #[reactor(max_in_flight = 4)]
    async fn enrich(t: &JobOpened, _ctx: Ctx<'_>) -> Result<Events> {
        let _ = t;
        Ok(Events::new())
    }
}

fn main() {}
