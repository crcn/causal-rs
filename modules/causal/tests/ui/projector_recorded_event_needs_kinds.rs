// A &RecordedEvent projector subscribes cross-kind — the subscription
// must be written out (`kinds = [...]`): each entry couples every
// workflow emitting that kind to this projector's write latency.
use anyhow::Result;
use causal::contexts::Ctx;
use causal::types::RecordedEvent;

#[causal::projectors]
mod bad {
    use super::*;

    #[projector(name = "ops.audit")]
    async fn audit(e: &RecordedEvent, _ctx: Ctx<'_>) -> Result<()> {
        let _ = e;
        Ok(())
    }
}

fn main() {}
