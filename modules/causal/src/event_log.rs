//! Append-only event log trait — the write side of the durable event store.
//!
//! Backends implement [`EventLogBackend`]; the `Engine` reads and writes
//! through this trait. The shapes match KurrentDB's primitives:
//!
//! - [`EventData`] / [`RecordedEvent`] mirror Kurrent's pre-/post-write
//!   event types.
//! - [`StreamState`] is the expected-state parameter on
//!   [`EventLogBackend::append_to_stream`] (matches Kurrent's enum
//!   exactly: `Any`, `NoStream`, `StreamExists`, `StreamRevision(u64)`).
//! - [`StreamRevision`] is the 0-indexed concrete revision of a recorded
//!   event.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    WriteResult, EventData, LogCursor, RecordedEvent, StreamRevision, StreamState,
};

/// Append-only event log. One write primitive — every event belongs to a
/// stream (`{category}-{subject_id}`); see [`append_to_stream`](Self::append_to_stream).
///
/// # Idempotency contract
///
/// `append_to_stream` MUST be idempotent on `event_id`: a second call whose
/// events carry already-persisted `event_id`s returns an equivalent
/// [`WriteResult`] without creating duplicate entries.
///
/// A dedup-hit MUST be a **byte-identical** redelivery: backends MUST
/// error (not silently keep the old row) when a redelivered event's
/// `payload`, `event_type`, `workflow_id`, or `causation_id` differs
/// from the persisted row. A divergent re-emission means the producer is
/// nondeterministic under redelivery (wall clock, rand, or an external
/// call not under `ctx.remember`) — silently keeping the old row while
/// the caller believes its new decision won lets state diverge invisibly
/// from intent. `created_at` and `metadata` are exempt: both are
/// documented hints that legitimate redeliveries may re-stamp.
///
/// **Placement is part of identity.** The same `event_id` redelivered to
/// a different stream — a different `category` or `subject_id` from the
/// persisted row — is also divergent (the producer routed one logical
/// event to two subjects, e.g. a reactor whose output `SUBJECT` changed)
/// and MUST error. Backends with a global `event_id` index (Memory,
/// Postgres) enforce this. Backends that dedup *per-stream* (Kurrent,
/// whose idempotency scan reads only the target stream's tail) cannot see
/// a cross-stream `event_id` reuse without a global index, so they do not
/// yet enforce placement identity — tracked by the idempotency-index work.
///
/// **`EventData::created_at` is a hint, not authoritative.** Backends MAY
/// override with a server-assigned timestamp on write (KurrentDB does this
/// unconditionally; `MemoryStore` preserves the client value). Consumers
/// reading `RecordedEvent::created_at` see whatever the backend persisted.
#[async_trait]
pub trait EventLogBackend: Send + Sync {
    /// Read events from `$all` after `after`, up to `limit` events
    /// ordered by position.
    async fn read_all(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<RecordedEvent>>;

    /// Read events from a single stream (`{category}-{subject_id}`). Pass
    /// `after: Some(revision)` to load only events with revision >
    /// the given value (snapshot + partial replay); `None` for full
    /// replay.
    async fn read_stream(
        &self,
        category: &str,
        subject_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>>;

    /// Latest global position in the log (`LogCursor::ZERO` if empty).
    async fn latest_position(&self) -> Result<LogCursor>;

    /// The append primitive — write `events` to stream
    /// `{category}-{subject_id}` under an optimistic-concurrency check.
    ///
    /// The batch is **atomic**: all events land at consecutive revisions
    /// or none do. This mirrors KurrentDB's `append_to_stream`, which takes
    /// a sequence of events and commits them as a unit — it's what lets a
    /// multi-fact decision ([`Engine::append`](crate::Engine::append))
    /// commit without a torn write. `events` must be non-empty.
    ///
    /// `expected` matches KurrentDB's [`StreamState`] semantics:
    /// `NoStream` for an empty stream's first event,
    /// `StreamRevision(n)` to assert the last event's revision is `n`,
    /// `StreamExists` for any non-empty stream, `Any` to skip the check
    /// (append-only fact streams; idempotency then rests on `event_id`).
    ///
    /// The returned [`WriteResult`] describes the **last** event in the
    /// batch (its position + revision); the rest occupy the consecutive
    /// revisions below it.
    ///
    /// **Idempotency precondition**: within one batch the `event_id`s must be
    /// either ALL new or ALL already-persisted — never a mix. Idempotent
    /// retries resubmit the *identical* batch, which is fine; submitting a
    /// batch that partially overlaps a prior write is a caller bug. Backends
    /// detect redelivery by the batch's last `event_id` and reject a
    /// partially-overlapping batch rather than corrupt the stream. (All
    /// in-tree callers satisfy this: `Engine::append` mints fresh ids per
    /// attempt; reactor outputs are single-event batches.)
    ///
    /// On a mismatch the backend returns an `anyhow::Error` carrying a
    /// [`ConflictError`] (downcast with `.downcast_ref::<ConflictError>()`).
    /// Command handlers SHOULD reload at the new `current` revision,
    /// re-decide, and retry — see [`Engine::append`](crate::Engine::append).
    ///
    /// Backends MUST make this atomic (Postgres transaction, in-memory
    /// single-mutex, KurrentDB native multi-event append).
    async fn append_to_stream(
        &self,
        category: &str,
        subject_id: Uuid,
        expected: StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult>;
}

/// Returned when [`EventLogBackend::append_to_stream`] sees a stream
/// state that doesn't match the caller's expectation. Command handlers
/// SHOULD reload the aggregate (at `current`) and retry the decision;
/// if the new state still permits the command, append again with the
/// fresh `StreamRevision(current.raw())`.
#[derive(Debug, thiserror::Error)]
#[error("aggregate stream state mismatch: expected {expected}, current {current:?}")]
pub struct ConflictError {
    pub expected: StreamState,
    pub current:  Option<StreamRevision>,
}

/// Returned by [`EventLogBackend::append_to_stream`] when a redelivered
/// event's `event_id` is already persisted but its content differs from
/// the stored row — the divergent-redelivery case of the idempotency
/// contract above. The backend keeps the persisted row and returns this.
///
/// It exists as a *typed* error (rather than a bare message) so the
/// reactor runner can tell divergence apart from genuine I/O failure by
/// `downcast_ref`, without string-matching. The distinction matters: a
/// divergent redelivery is, by construction, a dedup-hit — the producer's
/// canonical output already exists and was already consumed — so the
/// runner accepts the persisted row and shouts, where it would retry a
/// real infra error forever. Retrying divergence can never succeed (the
/// store keeps the original row, so every retry re-diverges), and parking
/// it would emit a terminal failure for work that *succeeded* and turn
/// every full replay of a nondeterministic reactor into a failure storm.
///
/// `diff` names where the rows differ — a JSON path like
/// `outputs[1].candidates[0].signal_id` where the backend computes one,
/// or the set of compared fields otherwise. The nondeterminism is usually
/// in a dependency far from the reactor body, and this is the difference
/// between a grep and a debugging session.
#[derive(Debug, thiserror::Error)]
#[error("append_to_stream: divergent redelivery for event_id {event_id} — \
         the persisted row differs from this batch's event ({diff}). A \
         dedup-hit must be byte-identical; a differing re-emission means the \
         producer is nondeterministic under redelivery (wall clock, rand, or \
         an external call not under ctx.effect). The persisted row is kept \
         unchanged.")]
pub struct DivergentRedelivery {
    pub event_id: Uuid,
    pub diff:     String,
    /// The canonical persisted row (the log's version of `event_id`) when the
    /// backend can supply it cheaply. Lets the reactor runner reconcile a
    /// sealed decision record to the log — the log is the source of truth, so
    /// on divergence the record is re-sealed to match, and a future redelivery
    /// replays a byte-identical (dedup-hit) batch instead of re-diverging.
    /// `None` when unavailable — the runner falls back to removing the
    /// contradicted record (no lying record persists; the body may re-run on a
    /// later redelivery). Boxed to keep the error type small.
    pub canonical: Option<Box<RecordedEvent>>,
}

/// Convenience over the single [`EventLogBackend::append_to_stream`]
/// primitive: append `event` to its own stream with [`StreamState::Any`]
/// (append-only, no concurrency check; idempotency rests on `event_id`).
///
/// The destination stream is `event.category` / `event.subject_id` when
/// carried; otherwise it's derived — category from the `{category}:{name}`
/// `event_type` prefix, subject_id from the event's own `event_id` (a
/// standalone single-event stream). So a bare fact always lands somewhere
/// sensible, never a shared `_global`.
///
/// Not a backend method — backends implement only `append_to_stream`.
/// Sugar for seeding fixtures and ad-hoc single appends. Invariant-bearing
/// writes go through [`Engine::append`](crate::Engine::append); the typed
/// emit/reactor paths set `category`/`subject_id` explicitly.
pub async fn append_event<B: EventLogBackend + ?Sized>(
    backend: &B,
    event: EventData,
) -> Result<WriteResult> {
    // Derive the placement category from the event_type prefix when it
    // is absent OR explicitly empty — an empty `Some("")` would match no
    // consumer (and, worse, act as a wildcard in `matches_category`), so
    // normalize it the same as `None`.
    let category = event
        .category
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| {
            let derived = event.event_type.as_str();
            if derived.is_empty() { "event" } else { derived }.to_string()
        });
    let subject_id = event.subject_id.unwrap_or(event.event_id);
    backend
        .append_to_stream(&category, subject_id, StreamState::Any, vec![event])
        .await
}

/// First differing JSON path between two values (e.g.
/// `outputs[2].candidates[0].signal_id`), or `None` when equal.
///
/// Divergent-redelivery errors print this: the producer's
/// nondeterminism is usually in a dependency crate far from the
/// reactor body, and the path is the difference between a grep and a
/// debugging session. Backends share it so all three stores report
/// the same way.
pub fn first_diff_path(a: &serde_json::Value, b: &serde_json::Value) -> Option<String> {
    fn walk(a: &serde_json::Value, b: &serde_json::Value, path: &mut String) -> bool {
        use serde_json::Value::*;
        match (a, b) {
            (Object(ma), Object(mb)) => {
                // Union of keys, in a's order then b-only keys.
                for (k, va) in ma {
                    let len = path.len();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(k);
                    match mb.get(k) {
                        Some(vb) => {
                            if walk(va, vb, path) {
                                return true;
                            }
                        }
                        None => return true, // key missing on one side
                    }
                    path.truncate(len);
                }
                for k in mb.keys() {
                    if !ma.contains_key(k) {
                        if !path.is_empty() {
                            path.push('.');
                        }
                        path.push_str(k);
                        return true;
                    }
                }
                false
            }
            (Array(va), Array(vb)) => {
                for (i, (ea, eb)) in va.iter().zip(vb.iter()).enumerate() {
                    let len = path.len();
                    path.push_str(&format!("[{i}]"));
                    if walk(ea, eb, path) {
                        return true;
                    }
                    path.truncate(len);
                }
                if va.len() != vb.len() {
                    path.push_str(&format!("[{}]", va.len().min(vb.len())));
                    return true;
                }
                false
            }
            _ => a != b,
        }
    }
    let mut path = String::new();
    if walk(a, b, &mut path) {
        Some(if path.is_empty() { "(root)".to_string() } else { path })
    } else {
        None
    }
}

#[cfg(test)]
mod diff_path_tests {
    use super::first_diff_path;
    use serde_json::json;

    #[test]
    fn equal_values_have_no_diff() {
        let v = json!({"a": [1, {"b": 2}]});
        assert_eq!(first_diff_path(&v, &v), None);
    }

    #[test]
    fn nested_diff_prints_the_full_path() {
        let a = json!({"outputs": [{"x": 1}, {"candidates": [{"signal_id": "s1"}]}]});
        let b = json!({"outputs": [{"x": 1}, {"candidates": [{"signal_id": "s2"}]}]});
        assert_eq!(
            first_diff_path(&a, &b).as_deref(),
            Some("outputs[1].candidates[0].signal_id"),
        );
    }

    #[test]
    fn missing_key_and_length_mismatch_are_located() {
        let a = json!({"k": {"present": 1}});
        let b = json!({"k": {}});
        assert_eq!(first_diff_path(&a, &b).as_deref(), Some("k.present"));

        let a = json!([1, 2, 3]);
        let b = json!([1, 2]);
        assert_eq!(first_diff_path(&a, &b).as_deref(), Some("[2]"));
    }

    #[test]
    fn scalar_root_diff_is_root() {
        assert_eq!(
            first_diff_path(&json!(1), &json!(2)).as_deref(),
            Some("(root)"),
        );
    }
}
