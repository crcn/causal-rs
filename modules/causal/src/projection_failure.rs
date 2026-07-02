//! Poison-park failure handling shared by the serial projection runners
//! ([`ProjectionRunner`](crate::projection_runner::ProjectionRunner) and
//! [`MultiProjectorRunner`](crate::multi_projector::MultiProjectorRunner)).
//!
//! Mirrors the reactor failure taxonomy ([`crate::failure`]): a
//! deterministically-failing `project`/fold is classified poison and
//! **parked** — a built-in [`PROJECTION_FAILED_KIND`] fact is appended to the
//! poison event's own subject history and the cursor advances past it — rather
//! than wedging the consumer forever on a `?`-propagated error.
//!
//! Self-contained retry accounting: a serial runner processes one event at a
//! time under a frozen cursor (the supervisor re-drives `step` from the same
//! position on `Err`), so a single slot tracks the currently-failing event.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::clock::Clock;
use crate::engine::WorkflowHighWater;
use crate::event_log::EventLogBackend;
use crate::failure::{classify_structural, ErrorClass, FailureClass, TRANSIENT_CEILING};
use crate::reactor::RetryPolicy;
use crate::reactor_observer::ReactorObserver;
use crate::reactor_runner::derive_output_event_id;
use crate::types::{EventData, RecordedEvent, StreamState};

/// The built-in terminal fact a projector/multi-projector appends when it
/// parks a poison event — mirrors [`crate::reactor_runner::REACTION_FAILED_KIND`].
/// Appended to the poison event's own subject history; surfaced as a parked
/// projection failure, never retried.
pub const PROJECTION_FAILED_KIND: &str = "causal:projection_failed";

/// What [`FailureState::on_failure`] decided: park now (with the failure
/// class) or keep retrying (propagate the `Err` so the supervisor backs off).
pub(crate) enum FailureDecision {
    /// Park as a terminal failure with this class and attempt count.
    Park { class: FailureClass, attempts: u32 },
    /// Not terminal yet — transient within the ceiling, or under the
    /// bounded-attempt budget. Propagate the error and retry.
    Retry,
}

/// Per-runner failure policy + in-flight retry accounting. One instance per
/// runner, shared by both projection runners.
pub(crate) struct FailureState {
    pub retry_policy: RetryPolicy,
    pub clock: Arc<dyn Clock>,
    pub settle_tracker: Option<WorkflowHighWater>,
    /// `(event_id, attempts-including-this-one)` for the currently-failing
    /// event. Reset when the failing event changes or the event succeeds.
    attempts: parking_lot::Mutex<Option<(Uuid, u32)>>,
    /// `(event_id, first-transient-instant)` — the liveness-time origin for a
    /// transient failure, so the ceiling is measured from the first blip.
    transient_since: parking_lot::Mutex<Option<(Uuid, tokio::time::Instant)>>,
}

impl FailureState {
    pub fn new(retry_policy: RetryPolicy, clock: Arc<dyn Clock>) -> Self {
        Self {
            retry_policy,
            clock,
            settle_tracker: None,
            attempts: parking_lot::Mutex::new(None),
            transient_since: parking_lot::Mutex::new(None),
        }
    }

    /// Record one failure of `event_id` and decide park vs retry, mirroring
    /// the reactor decision table (`reactor_runner.rs`): poison parks
    /// immediately; transient retries up to the liveness-time ceiling then
    /// parks as `transient_exhausted`; domain/unclassified retry up to
    /// `max_attempts` then park.
    pub fn on_failure(&self, event_id: Uuid, err: &anyhow::Error) -> FailureDecision {
        // Bump the attempt counter for this event (reset on a new event).
        let attempts = {
            let mut slot = self.attempts.lock();
            let n = match *slot {
                Some((id, n)) if id == event_id => n + 1,
                _ => 1,
            };
            *slot = Some((event_id, n));
            n
        };

        match classify_structural(err) {
            Some(ErrorClass::Poison) => FailureDecision::Park {
                class: FailureClass::Poison,
                attempts,
            },
            Some(ErrorClass::Transient) => {
                let mut slot = self.transient_since.lock();
                let since = match *slot {
                    Some((id, t)) if id == event_id => t,
                    _ => {
                        let t = tokio::time::Instant::now();
                        *slot = Some((event_id, t));
                        t
                    }
                };
                if since.elapsed() >= TRANSIENT_CEILING {
                    FailureDecision::Park {
                        class: FailureClass::TransientExhausted,
                        attempts,
                    }
                } else {
                    FailureDecision::Retry
                }
            }
            Some(ErrorClass::Domain) => {
                if attempts >= self.retry_policy.max_attempts {
                    FailureDecision::Park {
                        class: FailureClass::Domain,
                        attempts,
                    }
                } else {
                    FailureDecision::Retry
                }
            }
            None => {
                if attempts >= self.retry_policy.max_attempts {
                    FailureDecision::Park {
                        class: FailureClass::Unclassified,
                        attempts,
                    }
                } else {
                    FailureDecision::Retry
                }
            }
        }
    }

    /// Clear per-event accounting — on success, or after parking.
    pub fn clear(&self, event_id: Uuid) {
        let mut a = self.attempts.lock();
        if matches!(*a, Some((id, _)) if id == event_id) {
            *a = None;
        }
        let mut t = self.transient_since.lock();
        if matches!(*t, Some((id, _)) if id == event_id) {
            *t = None;
        }
    }

    /// Append the built-in terminal fact and notify the observer. The caller
    /// advances the cursor afterwards (park + advance). Appending is idempotent
    /// under replay: the event_id is derived deterministically, so a replay
    /// re-parks to the same id and the backend dedupes.
    #[allow(clippy::too_many_arguments)]
    pub async fn park(
        &self,
        log: &dyn EventLogBackend,
        consumer_id: &str,
        event: &RecordedEvent,
        class: FailureClass,
        attempts: u32,
        error: String,
        observer: Option<&dyn ReactorObserver>,
    ) -> Result<()> {
        let now = self.clock.now();
        if let Some(obs) = observer {
            obs.projection_terminal_failure(
                event.event_id,
                consumer_id,
                event.workflow_id,
                attempts,
                &error,
                now,
            );
        }
        append_projection_failed(
            log,
            consumer_id,
            event,
            class,
            attempts,
            error,
            now,
            self.settle_tracker.as_ref(),
        )
        .await?;
        self.clear(event.event_id);
        Ok(())
    }
}

/// Build + append the `causal:projection_failed` fact to the poison event's
/// own subject history, with a deterministic id (idempotent under replay).
#[allow(clippy::too_many_arguments)]
async fn append_projection_failed(
    log: &dyn EventLogBackend,
    consumer_id: &str,
    event: &RecordedEvent,
    class: FailureClass,
    attempts: u32,
    error: String,
    now: DateTime<Utc>,
    settle_tracker: Option<&WorkflowHighWater>,
) -> Result<()> {
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "projector_id".to_string(),
        serde_json::Value::String(consumer_id.to_string()),
    );

    let out = EventData {
        // `nth = u32::MAX` keeps this distinct from any real projector output
        // of the same identity (paralleling the reactor terminal fact).
        event_id: derive_output_event_id(
            consumer_id,
            event.event_id,
            PROJECTION_FAILED_KIND,
            event.subject_id,
            u32::MAX,
        ),
        causation_id: Some(event.event_id),
        workflow_id: event.workflow_id,
        event_type: PROJECTION_FAILED_KIND.to_string(),
        payload: serde_json::json!({
            "consumer": consumer_id,
            "event_id": event.event_id,
            "event_type": event.event_type,
            "position": event.position.raw(),
            "class": class.as_str(),
            "error": error,
            "attempts": attempts,
        }),
        created_at: now,
        category: Some(event.category.clone()),
        subject_id: Some(event.subject_id),
        metadata,
        ephemeral: None,
        persistent: true,
    };
    let write = log
        .append_to_stream(
            &event.category,
            event.subject_id,
            StreamState::Any,
            vec![out],
        )
        .await?;
    if let Some(tracker) = settle_tracker {
        tracker
            .lock()
            .unwrap()
            .bump(event.workflow_id, write.position);
    }
    Ok(())
}
