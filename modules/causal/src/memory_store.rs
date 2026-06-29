//! In-memory backend: `EventLogBackend` + `CheckpointStore` +
//! `ReactorCheckpoint` + `SnapshotStore` (+ `ReactorObserver` and the
//! inspector read surface).
//!
//! Suitable for tests, examples, and single-process use cases. Drop in
//! a Postgres / Kurrent backend for production durability.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::checkpoint_store::ReactorCheckpoint;
use crate::reactor_observer::ReactorObserver;
use crate::types::*;

/// In-memory cursor row for one projection.
#[derive(Clone)]
struct ProjectionCursorEntry {
    cursor: LogCursor,
}

/// Derived indices over the global log, maintained by
/// `append_to_stream`. They exist so the hot paths stay sub-linear as
/// the log grows into the millions — without them, every append paid an
/// O(N) stream-count scan plus an O(N) dedup scan (quadratic total),
/// and `read_stream` walked the whole log per call.
///
/// **Lock ordering:** always acquire `global_log` BEFORE `log_index`.
/// The index is only ever written under both locks, so a reader holding
/// the log lock sees an index consistent with the Vec.
#[derive(Default)]
struct LogIndex {
    /// event_id → offset in the log Vec (dedup + divergence checks).
    by_event_id: HashMap<Uuid, usize>,
    /// (category, subject_id) → that stream's log offsets, in revision
    /// order. `len()` is the stream's event count; offset `r` holds
    /// revision `r` (revisions are dense per stream).
    streams: HashMap<(String, Uuid), Vec<usize>>,
}

/// Read-only lock guard over the global event log, returned by
/// [`MemoryStore::global_log`]. Derefs to `[RecordedEvent]`; no
/// mutable access exists, because the store's [`LogIndex`] is derived
/// from this Vec and external mutation would silently desync it.
///
/// **This holds the log lock.** Copy what you need and drop it before
/// calling any store method (`read_all`, `append_to_stream`, ...) —
/// the lock is not reentrant, so calling back into the store while
/// holding the guard deadlocks. Holding it across an `.await` blocks
/// every writer for the duration.
pub struct GlobalLogGuard<'a>(parking_lot::MutexGuard<'a, Vec<RecordedEvent>>);

impl std::ops::Deref for GlobalLogGuard<'_> {
    type Target = [RecordedEvent];
    fn deref(&self) -> &[RecordedEvent] {
        &self.0
    }
}

/// In-memory backend implementing the full trait surface.
#[derive(Clone)]
pub struct MemoryStore {
    /// Global event log.
    global_log: Arc<Mutex<Vec<RecordedEvent>>>,
    /// Derived indices over `global_log` — see [`LogIndex`].
    log_index: Arc<Mutex<LogIndex>>,
    /// Global position counter for event ordering.
    global_position: Arc<AtomicU64>,
    /// Snapshot store keyed by (aggregate_type, aggregate_id).
    snapshots: Arc<DashMap<(String, Uuid), Snapshot>>,
    /// Per-projection cursor + status.
    projection_cursors: Arc<DashMap<String, ProjectionCursorEntry>>,
    /// terminal-failure attempt counter keyed by (consumer_id, trigger_id).
    /// Survives ReactorRunner reconstruction within the store's
    /// lifetime; lost on process crash (matches MemoryStore's
    /// "no durability" position).
    reactor_attempts: Arc<DashMap<(String, Uuid), u32>>,

    // ── Inspector observability ──────────────────────────
    //
    // Populated by `impl ReactorObserver for MemoryStore`. Read by
    // `causal_inspector` to render UI panes.
    //
    /// Reactor execution timing: `(event_id, reactor_id)` → `(wf,
    /// started_at, completed_at, status, error, attempts)`.
    reactor_executions:
        Arc<DashMap<(Uuid, String), (Uuid, DateTime<Utc>, Option<DateTime<Utc>>, String, Option<String>, i32)>>,
    /// Per-attempt history: `(event_id, reactor_id, wf, attempt#,
    /// status, error, started_at, completed_at)`.
    reactor_attempt_history:
        Arc<Mutex<Vec<(Uuid, String, Uuid, i32, String, Option<String>, DateTime<Utc>, DateTime<Utc>)>>>,
    /// Reactor log entries captured via `ctx.log(...)`:
    /// `(event_id, reactor_id, LogEntry)`.
    reactor_log_entries: Arc<Mutex<Vec<(Uuid, String, LogEntry)>>>,
    /// Aggregate state after each fold:
    /// `(wf, position, event_id, aggregate_key, state_json)`.
    aggregate_state_snapshots:
        Arc<Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>>>,
    /// Reactor describe-DSL output per event:
    /// `(wf, position, event_id, reactor_id, description_json)`.
    reactor_description_snapshots:
        Arc<Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            global_log: Arc::new(Mutex::new(Vec::new())),
            log_index: Arc::new(Mutex::new(LogIndex::default())),
            global_position: Arc::new(AtomicU64::new(1)),
            snapshots: Arc::new(DashMap::new()),
            projection_cursors: Arc::new(DashMap::new()),
            reactor_attempts: Arc::new(DashMap::new()),
            reactor_executions: Arc::new(DashMap::new()),
            reactor_attempt_history: Arc::new(Mutex::new(Vec::new())),
            reactor_log_entries: Arc::new(Mutex::new(Vec::new())),
            aggregate_state_snapshots: Arc::new(Mutex::new(Vec::new())),
            reactor_description_snapshots: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Read-only view of the global event log (test assertions, the
    /// inspector's read surface).
    ///
    /// Read-only **by construction**: `append_to_stream` maintains
    /// derived indices over this Vec (see [`LogIndex`]), so external
    /// mutation would silently desync them and corrupt dedup/stream
    /// reads later. Pre-0.9 this returned the raw `&Mutex<Vec<_>>` —
    /// callers migrate by dropping the `.lock()` call.
    pub fn global_log(&self) -> GlobalLogGuard<'_> {
        GlobalLogGuard(self.global_log.lock())
    }

    // ── Inspector accessors ──────────────────────────────────────
    //
    // Populated by the `ReactorObserver` impl below as the engine
    // calls hooks; consumed by `causal_inspector`.

    /// Reactor execution timing records keyed by `(event_id, reactor_id)`.
    pub fn reactor_executions(
        &self,
    ) -> &DashMap<(Uuid, String), (Uuid, DateTime<Utc>, Option<DateTime<Utc>>, String, Option<String>, i32)> {
        &self.reactor_executions
    }

    /// Per-attempt history. Each row is one `react()` invocation.
    pub fn reactor_attempt_history(
        &self,
    ) -> &Mutex<Vec<(Uuid, String, Uuid, i32, String, Option<String>, DateTime<Utc>, DateTime<Utc>)>> {
        &self.reactor_attempt_history
    }

    /// Log entries pushed by reactor bodies via `ctx.log(...)`.
    pub fn reactor_log_entries(&self) -> &Mutex<Vec<(Uuid, String, LogEntry)>> {
        &self.reactor_log_entries
    }

    /// Aggregate-state-after-each-event snapshots.
    pub fn aggregate_state_snapshots(
        &self,
    ) -> &Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>> {
        &self.aggregate_state_snapshots
    }

    /// Reactor describe-DSL output captured per-event.
    pub fn reactor_description_snapshots(
        &self,
    ) -> &Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>> {
        &self.reactor_description_snapshots
    }
}

// ── ReactorObserver implementation ──────────────────────────────────

impl ReactorObserver for MemoryStore {
    fn reactor_started(
        &self,
        event_id: Uuid,
        reactor_id: &str,
        workflow_id: Uuid,
        attempt: u32,
        started_at: DateTime<Utc>,
    ) {
        self.reactor_executions.insert(
            (event_id, reactor_id.to_string()),
            (workflow_id, started_at, None, "running".to_string(), None, attempt as i32),
        );
    }

    fn reactor_completed(
        &self,
        event_id: Uuid,
        reactor_id: &str,
        workflow_id: Uuid,
        attempt: u32,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
        logs: &[LogEntry],
    ) {
        // Update execution timing record (latest known state per event_id+reactor_id).
        if let Some(mut entry) =
            self.reactor_executions.get_mut(&(event_id, reactor_id.to_string()))
        {
            entry.2 = Some(completed_at);
            entry.3 = "completed".to_string();
            entry.5 = attempt as i32;
        }
        // Append a closed attempt row.
        self.reactor_attempt_history.lock().push((
            event_id,
            reactor_id.to_string(),
            workflow_id,
            attempt as i32,
            "completed".to_string(),
            None,
            started_at,
            completed_at,
        ));
        // Capture log entries from this attempt.
        if !logs.is_empty() {
            let mut sink = self.reactor_log_entries.lock();
            for e in logs {
                sink.push((event_id, reactor_id.to_string(), e.clone()));
            }
        }
    }

    fn reactor_failed(
        &self,
        event_id: Uuid,
        reactor_id: &str,
        workflow_id: Uuid,
        attempt: u32,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
        error: &str,
        logs: &[LogEntry],
    ) {
        // Update execution record — surface error + attempt count.
        if let Some(mut entry) =
            self.reactor_executions.get_mut(&(event_id, reactor_id.to_string()))
        {
            entry.3 = "retry".to_string();
            entry.4 = Some(error.to_string());
            entry.5 = attempt as i32;
        }
        self.reactor_attempt_history.lock().push((
            event_id,
            reactor_id.to_string(),
            workflow_id,
            attempt as i32,
            "retry".to_string(),
            Some(error.to_string()),
            started_at,
            completed_at,
        ));
        if !logs.is_empty() {
            let mut sink = self.reactor_log_entries.lock();
            for e in logs {
                sink.push((event_id, reactor_id.to_string(), e.clone()));
            }
        }
    }

    fn reactor_terminal_failure(
        &self,
        event_id: Uuid,
        reactor_id: &str,
        _workflow_id: Uuid,
        attempts: u32,
        error: &str,
        at: DateTime<Utc>,
    ) {
        if let Some(mut entry) =
            self.reactor_executions.get_mut(&(event_id, reactor_id.to_string()))
        {
            entry.2 = Some(at);
            entry.3 = "error".to_string();
            entry.4 = Some(error.to_string());
            entry.5 = attempts as i32;
        }
    }

    fn aggregate_folded(
        &self,
        workflow_id: Uuid,
        position: LogCursor,
        event_id: Uuid,
        aggregate_key: &str,
        state: serde_json::Value,
    ) {
        self.aggregate_state_snapshots.lock().push((
            workflow_id,
            position.raw(),
            event_id,
            aggregate_key.to_string(),
            state,
        ));
    }

    fn reactor_description(
        &self,
        workflow_id: Uuid,
        position: LogCursor,
        event_id: Uuid,
        reactor_id: &str,
        description: serde_json::Value,
    ) {
        self.reactor_description_snapshots.lock().push((
            workflow_id,
            position.raw(),
            event_id,
            reactor_id.to_string(),
            description,
        ));
    }
}

// ── EventLogBackend implementation ──────────────────────────────────

#[async_trait]
impl crate::event_log::EventLogBackend for MemoryStore {
    async fn read_all(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<RecordedEvent>> {
        let log = self.global_log.lock();
        // Positions ascend with Vec order (assigned under this lock),
        // so the first event past `after` is found by binary search —
        // O(log N + limit), not an O(N) front-scan per poll.
        let start = log.partition_point(|e| e.position <= after);
        Ok(log[start..].iter().take(limit).cloned().collect())
    }

    async fn read_stream(
        &self,
        category: &str,
        subject_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        let log = self.global_log.lock();
        let idx = self.log_index.lock(); // lock ordering: log → index
        let Some(offsets) = idx.streams.get(&(category.to_string(), subject_id)) else {
            return Ok(Vec::new());
        };
        // Offset `r` holds revision `r` (dense per stream), so
        // `revision > min` is a suffix starting at `min + 1`.
        let skip = after.map(|min| min.raw().saturating_add(1) as usize).unwrap_or(0);
        Ok(offsets[skip.min(offsets.len())..]
            .iter()
            .map(|&i| log[i].clone())
            .collect())
    }

    async fn latest_position(&self) -> Result<LogCursor> {
        let log = self.global_log.lock();
        Ok(log.last().map(|e| e.position).unwrap_or(LogCursor::ZERO))
    }

    /// Atomic CAS append of a batch. Holds the global log mutex for the
    /// duration of the state check + inserts so two concurrent callers
    /// can't both pass the check, and so the whole batch lands or none of
    /// it does. Returns the [`WriteResult`] for the last event.
    async fn append_to_stream(
        &self,
        category: &str,
        subject_id: Uuid,
        expected: crate::types::StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult> {
        use crate::types::StreamState;
        let Some(last_id) = events.last().map(|e| e.event_id) else {
            anyhow::bail!("append_to_stream: events must be non-empty");
        };

        // Within-batch event_id uniqueness. The durable backends enforce
        // this via `UNIQUE(event_id)`; MemoryStore must match or a batch
        // carrying a duplicate id would persist both rows and silently
        // undermine the event_id-dedup idempotency the whole runtime
        // rests on (a later redelivery dedups on the last id and skips,
        // making the duplicate permanent).
        if events.len() > 1 {
            for (i, e) in events.iter().enumerate() {
                if events[i + 1..].iter().any(|o| o.event_id == e.event_id) {
                    anyhow::bail!(
                        "append_to_stream: batch contains duplicate event_id {} — \
                         event_ids must be unique within a batch",
                        e.event_id,
                    );
                }
            }
        }

        let mut log = self.global_log.lock();
        let mut idx = self.log_index.lock(); // lock ordering: log → index

        // Idempotency: a batch is written atomically, so if the last
        // event_id is already present the whole batch is — return its
        // result regardless of expected state.
        //
        // BUT a dedup-hit must be a *byte-identical* redelivery. A
        // divergent payload means the producer re-decided differently on
        // redelivery (nondeterministic react body: wall clock, rand, an
        // un-remember()ed external call). Returning Ok while keeping the
        // old row would let the caller believe its new decision won —
        // state diverging silently from intent. Scream instead; the fix
        // is always upstream (make the producer deterministic).
        // `created_at` is exempt: it's documented as a hint and legit
        // redeliveries re-stamp it. `metadata` is exempt for the same
        // reason (engine defaults may differ across deploys).
        if let Some(&existing_at) = idx.by_event_id.get(&last_id) {
            for e in &events {
                let Some(&at) = idx.by_event_id.get(&e.event_id) else {
                    anyhow::bail!(
                        "append_to_stream: partial-overlap batch — the batch tail \
                         {last_id} is persisted but event_id {} is not (event_ids \
                         must be all-new or all-already-persisted)",
                        e.event_id,
                    );
                };
                let row = &log[at];
                if row.payload != e.payload
                    || row.event_type != e.event_type
                    || row.workflow_id != e.workflow_id
                    || row.causation_id != e.causation_id
                {
                    // Name WHERE it diverged — the nondeterminism is
                    // usually in a dependency far from the reactor, and
                    // the JSON path is the difference between a grep
                    // and a debugging session.
                    let where_ = if row.payload != e.payload {
                        format!(
                            "payload at `{}`",
                            crate::event_log::first_diff_path(&row.payload, &e.payload)
                                .unwrap_or_else(|| "(root)".into()),
                        )
                    } else if row.event_type != e.event_type {
                        format!("event_type ({} vs {})", row.event_type, e.event_type)
                    } else if row.workflow_id != e.workflow_id {
                        "workflow_id".to_string()
                    } else {
                        "causation_id".to_string()
                    };
                    // Typed so the reactor runner can tell this apart from
                    // genuine I/O by downcast (it accepts the persisted row
                    // and shouts rather than retrying forever). The message
                    // is preserved on the error's Display.
                    return Err(anyhow::Error::new(
                        crate::event_log::DivergentRedelivery {
                            event_id: e.event_id,
                            diff:     where_,
                        },
                    ));
                }
            }
            let existing = &log[existing_at];
            return Ok(WriteResult {
                position: existing.position,
                revision: existing.revision,
            });
        }

        // Partial-overlap guard (see EventLogBackend::append_to_stream
        // contract): the last id is absent (checked above), so if any earlier
        // id is already present this is a torn/partial batch — reject rather
        // than double-write. Only multi-event batches can be torn.
        if events.len() > 1
            && events.iter().any(|e| idx.by_event_id.contains_key(&e.event_id))
        {
            anyhow::bail!(
                "append_to_stream: partial-overlap batch — an event_id already \
                 exists but the batch tail does not (event_ids must be all-new \
                 or all-already-persisted)"
            );
        }

        let stream_key = (category.to_string(), subject_id);
        let count = idx
            .streams
            .get(&stream_key)
            .map(|offsets| offsets.len() as u64)
            .unwrap_or(0);
        let current_tail: Option<StreamRevision> = if count == 0 {
            None
        } else {
            Some(StreamRevision::from_raw(count - 1))
        };
        let matches = match (expected, current_tail) {
            (StreamState::Any, _) => true,
            (StreamState::NoStream, None) => true,
            (StreamState::StreamExists, Some(_)) => true,
            (StreamState::StreamRevision(want), Some(actual)) => actual.raw() == want,
            _ => false,
        };
        if !matches {
            return Err(anyhow::Error::new(crate::event_log::ConflictError {
                expected,
                current: current_tail,
            }));
        }

        // Append all events at consecutive revisions/positions.
        let mut result = WriteResult {
            position: LogCursor::ZERO,
            revision: StreamRevision::from_raw(0),
        };
        for (offset, event) in events.into_iter().enumerate() {
            let position =
                LogCursor::from_raw(self.global_position.fetch_add(1, Ordering::SeqCst));
            let new_revision = StreamRevision::from_raw(count + offset as u64);
            idx.by_event_id.insert(event.event_id, log.len());
            idx.streams.entry(stream_key.clone()).or_default().push(log.len());
            log.push(RecordedEvent {
                position,
                event_id: event.event_id,
                causation_id: event.causation_id,
                workflow_id: event.workflow_id,
                event_type: event.event_type,
                payload: event.payload,
                created_at: event.created_at,
                category: category.to_string(),
                subject_id,
                revision: new_revision,
                metadata: event.metadata,
                ephemeral: event.ephemeral,
                persistent: event.persistent,
            });
            result = WriteResult { position, revision: new_revision };
        }

        Ok(result)
    }
}

#[async_trait]
impl crate::snapshot_store::SnapshotStore for MemoryStore {
    async fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Result<Option<Snapshot>> {
        let key = (aggregate_type.to_string(), aggregate_id);
        Ok(self.snapshots.get(&key).map(|v| v.value().clone()))
    }

    async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        let key = (snapshot.aggregate_type.clone(), snapshot.aggregate_id);
        self.snapshots.insert(key, snapshot);
        Ok(())
    }

    async fn delete_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Result<()> {
        self.snapshots.remove(&(aggregate_type.to_string(), aggregate_id));
        Ok(())
    }
}

#[async_trait]
impl crate::checkpoint_store::CheckpointStore for MemoryStore {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        Ok(self.projection_cursors.get(consumer_id).map(|e| e.cursor))
    }

    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        use dashmap::mapref::entry::Entry;
        match self.projection_cursors.entry(consumer_id.to_string()) {
            Entry::Occupied(mut slot) => { slot.get_mut().cursor = pos; }
            Entry::Vacant(slot) => {
                slot.insert(ProjectionCursorEntry { cursor: pos });
            }
        }
        Ok(())
    }
}

// ── ReactorCheckpoint implementation (C12 atomicity) ────────────────────

#[async_trait]
impl ReactorCheckpoint for MemoryStore {
    async fn record_reactor_attempt(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<u32> {
        let key = (consumer_id.to_string(), trigger_id);
        let mut entry = self.reactor_attempts.entry(key).or_insert(0);
        *entry += 1;
        Ok(*entry)
    }

    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<()> {
        let key = (consumer_id.to_string(), trigger_id);
        self.reactor_attempts.remove(&key);
        Ok(())
    }
}

#[cfg(test)]
mod append_tests {
    use super::*;
    use crate::event_log::EventLogBackend;

    fn ev(event_id: Uuid) -> EventData {
        EventData {
            event_id,
            causation_id: None,
            workflow_id: Uuid::new_v4(),
            event_type: "test:thing".into(),
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            category: Some("test".into()),
            subject_id: Some(Uuid::nil()),
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        }
    }

    #[tokio::test]
    async fn within_batch_duplicate_event_ids_are_rejected() {
        // Regression (adversarial-input fuzzer): a batch carrying the
        // same event_id twice once persisted both rows, silently
        // breaking the event_id-dedup idempotency the runtime rests on.
        // The durable backends reject it via UNIQUE(event_id); MemoryStore
        // must match.
        let store = MemoryStore::new();
        let dup = Uuid::new_v4();
        let err = EventLogBackend::append_to_stream(
            &store, "test", Uuid::nil(), StreamState::Any,
            vec![ev(dup), ev(Uuid::new_v4()), ev(dup)],
        ).await.unwrap_err();
        assert!(err.to_string().contains("duplicate event_id"), "got: {err:#}");

        // The whole batch rolled back — nothing persisted.
        let all = EventLogBackend::read_all(&store, LogCursor::ZERO, 100).await.unwrap();
        assert!(all.is_empty(), "rejected batch must not partially write");
    }

    #[tokio::test]
    async fn byte_identical_redelivery_dedups_to_original_write() {
        // The idempotency contract's happy path: a crash between append
        // and checkpoint redelivers the IDENTICAL batch; the backend
        // returns the original WriteResult without a second row.
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        let event = ev(id);
        let first = EventLogBackend::append_to_stream(
            &store, "test", Uuid::nil(), StreamState::Any, vec![event.clone()],
        ).await.unwrap();
        let second = EventLogBackend::append_to_stream(
            &store, "test", Uuid::nil(), StreamState::Any, vec![event],
        ).await.unwrap();
        assert_eq!(first.position, second.position);
        let all = EventLogBackend::read_all(&store, LogCursor::ZERO, 100).await.unwrap();
        assert_eq!(all.len(), 1, "redelivery must not create a second row");
    }

    #[tokio::test]
    async fn divergent_payload_redelivery_errors_loudly() {
        // A dedup-hit whose payload DIFFERS from the persisted row is
        // always an upstream determinism violation — a reactor that
        // re-decided differently on redelivery (wall clock, rand, an
        // un-remember()ed external call). Silently keeping the old row
        // while the caller believes the new decision won is how state
        // diverges invisibly. The backend must scream instead.
        let store = MemoryStore::new();
        let id = Uuid::new_v4();
        let mut a = ev(id);
        a.payload = serde_json::json!({ "decision": "ship" });
        EventLogBackend::append_to_stream(
            &store, "test", Uuid::nil(), StreamState::Any, vec![a],
        ).await.unwrap();

        let mut b = ev(id);
        b.payload = serde_json::json!({ "decision": "cancel" });
        let err = EventLogBackend::append_to_stream(
            &store, "test", Uuid::nil(), StreamState::Any, vec![b],
        ).await.unwrap_err();
        assert!(
            err.to_string().contains("divergent"),
            "divergent redelivery must error loudly, got: {err:#}",
        );

        // The original row is untouched.
        let all = EventLogBackend::read_all(&store, LogCursor::ZERO, 100).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].payload, serde_json::json!({ "decision": "ship" }));
    }

    #[tokio::test]
    async fn distinct_event_ids_in_a_batch_still_work() {
        let store = MemoryStore::new();
        EventLogBackend::append_to_stream(
            &store, "test", Uuid::nil(), StreamState::Any,
            vec![ev(Uuid::new_v4()), ev(Uuid::new_v4()), ev(Uuid::new_v4())],
        ).await.unwrap();
        let all = EventLogBackend::read_all(&store, LogCursor::ZERO, 100).await.unwrap();
        assert_eq!(all.len(), 3);
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::checkpoint_store::CheckpointStore;

    #[tokio::test]
    async fn cursor_set_and_get_round_trips() {
        let store = MemoryStore::new();
        let pos = LogCursor::from_raw(100);

        CheckpointStore::set(&store, "consumer_a", pos).await.unwrap();
        assert_eq!(
            CheckpointStore::get(&store, "consumer_a").await.unwrap(),
            Some(pos),
        );
    }

}
