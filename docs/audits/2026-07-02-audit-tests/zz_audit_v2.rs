// Scratchpad probe (NOT placed in the repo — the defect is in feature-gated
// causal_replay Kurrent code, unreachable from modules/causal/tests).
// Cargo.toml deps: causal (path), causal_replay (path, default-features=false,
// features=["kurrent"]), tokio full, anyhow, uuid v4, serde_json, chrono.
//
// Method: point the backend at a DEAD server (nothing listens on 127.0.0.1:1).
// Verifying a redelivery requires reading the persisted row, which must
// contact the server. Pre-register the batch id, then redeliver the same
// event_id with different payload/event_type/workflow/causation AND a
// different target stream.
// CORRECT behavior: typed DivergentRedelivery (or at minimum a network
// error/timeout proving verification was attempted).
// DEFECT: Ok(WriteResult) with the fabricated registry coordinates, no I/O.

use std::sync::Arc;
use std::time::Duration;

use causal::event_id_registry::{EventIdEntry, EventIdRegistry, InMemoryEventIdRegistry};
use causal::types::{EventData, LogCursor, StreamRevision, StreamState};
use causal::EventLogBackend;
use causal_replay::KurrentEventLogBackend;
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let backend = KurrentEventLogBackend::connect("kurrentdb://127.0.0.1:1?tls=false")?;
    let registry = Arc::new(InMemoryEventIdRegistry::new());
    let backend =
        backend.with_event_id_registry(registry.clone() as Arc<dyn EventIdRegistry>);

    // Simulate the original append: id X registered at position 42 / rev 7
    // (originally in some other stream with some other payload — the
    // registry stores neither).
    let id = Uuid::new_v4();
    registry
        .register(&[EventIdEntry {
            event_id: id,
            stream_position: LogCursor::from_raw(42),
            stream_revision: StreamRevision::from_raw(7),
        }])
        .await?;

    // Redeliver id X with EVERYTHING different.
    let subject = Uuid::new_v4();
    let divergent = EventData {
        event_id: id,
        causation_id: Some(Uuid::new_v4()),
        workflow_id: Uuid::new_v4(),
        event_type: "totally:different".into(),
        payload: serde_json::json!({"amount": 999_999, "attacker": true}),
        created_at: chrono::Utc::now(),
        category: Some("elsewhere".into()),
        subject_id: Some(subject),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };

    let res = tokio::time::timeout(
        Duration::from_secs(10),
        backend.append_to_stream("elsewhere", subject, StreamState::Any, vec![divergent]),
    )
    .await;

    match res {
        Err(_) => println!("TIMEOUT: verification was at least ATTEMPTED. Not proven."),
        Ok(Err(e)) => {
            let typed = e
                .downcast_ref::<causal::event_log::DivergentRedelivery>()
                .is_some();
            println!("ERROR (typed DivergentRedelivery: {typed}): {e:#} — not proven.");
        }
        Ok(Ok(w)) => println!(
            "DEFECT CONFIRMED: divergent same-id append to a DIFFERENT stream \
             returned Ok(position={}, revision={}) — the fabricated registry \
             coordinates — with zero verification and zero network I/O (the \
             server does not exist). DivergentRedelivery is unreachable on \
             this path.",
            w.position.raw(),
            w.revision.raw(),
        ),
    }
    Ok(())
}
