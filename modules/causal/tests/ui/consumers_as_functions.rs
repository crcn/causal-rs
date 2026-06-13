// Primitive 7: consumers are functions. The module macros remove trait
// ceremony only — trigger types come from fn signatures (wrongness is
// loud), names and knobs stay explicit (wrongness would be silent).
use anyhow::Result;
use causal::contexts::Ctx;
use causal::reactor::Events;
use causal::types::RecordedEvent;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[causal::event(name = "m_job_opened", subject_id = "job_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOpened { pub job_id: Uuid }

#[causal::event(name = "m_job_done", subject_id = "job_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDone { pub job_id: Uuid }

// ── reactors, with deps ──────────────────────────────────────────────
#[causal::reactors(deps = JobDeps)]
mod job_reactors {
    use super::*;

    pub struct JobDeps { pub suffix: String }

    #[reactor(name = "m.finish", ordering = per_workflow, max_in_flight = 2)]
    async fn finish(deps: &JobDeps, t: &JobOpened, _ctx: Ctx<'_>) -> Result<Events> {
        let _ = &deps.suffix;
        Ok(causal::events![JobDone { job_id: t.job_id }])
    }

    // A bare helper fn is left untouched.
    #[allow(dead_code)]
    fn helper() -> u32 { 7 }
}

// ── reactors, no deps ────────────────────────────────────────────────
#[causal::reactors]
mod plain_reactors {
    use super::*;

    #[reactor(name = "m.echo")]
    async fn echo(t: &JobDone, _ctx: Ctx<'_>) -> Result<Events> {
        let _ = t;
        Ok(Events::new())
    }
}

// ── projectors: typed + cross-kind in one module ─────────────────────
#[causal::projectors(deps = Sink)]
mod job_projectors {
    use super::*;

    pub struct Sink;

    #[projector(name = "m.rows")]
    async fn rows(_deps: &Sink, e: &JobOpened, _ctx: Ctx<'_>) -> Result<()> {
        let _ = e;
        Ok(())
    }

    #[projector(name = "m.audit", kinds = ["m_job_opened", "m_job_done"])]
    async fn audit(_deps: &Sink, e: &RecordedEvent, _ctx: Ctx<'_>) -> Result<()> {
        let _ = &e.event_type;
        Ok(())
    }
}

fn main() {
    // Registration lists exist with the right shapes.
    let rs: Vec<Box<dyn causal::ReactorRegistration>> =
        job_reactors::reactors(job_reactors::JobDeps { suffix: "x".into() });
    assert_eq!(rs.len(), 1);
    let rs2: Vec<Box<dyn causal::ReactorRegistration>> = plain_reactors::reactors();
    assert_eq!(rs2.len(), 1);
    let ps: Vec<Box<dyn causal::ProjectorRegistration>> =
        job_projectors::projectors(job_projectors::Sink);
    assert_eq!(ps.len(), 1);
    let ms: Vec<Box<dyn causal::MultiProjectorRegistration>> =
        job_projectors::multi_projectors(job_projectors::Sink);
    assert_eq!(ms.len(), 1);
}
