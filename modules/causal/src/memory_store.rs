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

/// In-memory backend implementing the full trait surface.
#[derive(Clone)]
pub struct MemoryStore {
    /// Global event log.
    global_log: Arc<Mutex<Vec<RecordedEvent>>>,
    /// Global position counter for event ordering.
    global_position: Arc<AtomicU64>,
    /// Snapshot store keyed by (aggregate_type, aggregate_id).
    snapshots: Arc<DashMap<(String, Uuid), Snapshot>>,
    /// Per-projection cursor + status.
    projection_cursors: Arc<DashMap<String, ProjectionCursorEntry>>,
    /// DLQ attempt counter keyed by (consumer_id, source_event_id).
    /// Survives ReactorRunner reconstruction within the store's
    /// lifetime; lost on process crash (matches MemoryStore's
    /// "no durability" position).
    reactor_attempts: Arc<DashMap<(String, Uuid), u32>>,

    // ── Inspector observability ──────────────────────────
    //
    // Populated by `impl ReactorObserver for MemoryStore`. Read by
    // `causal_inspector` to render UI panes.
    //
    /// Reactor execution timing: `(event_id, reactor_id)` → `(corr,
    /// started_at, completed_at, status, error, attempts)`.
    reactor_executions:
        Arc<DashMap<(Uuid, String), (Uuid, DateTime<Utc>, Option<DateTime<Utc>>, String, Option<String>, i32)>>,
    /// Per-attempt history: `(event_id, reactor_id, corr, attempt#,
    /// status, error, started_at, completed_at)`.
    reactor_attempt_history:
        Arc<Mutex<Vec<(Uuid, String, Uuid, i32, String, Option<String>, DateTime<Utc>, DateTime<Utc>)>>>,
    /// Reactor log entries captured via `ctx.log(...)`:
    /// `(event_id, reactor_id, LogEntry)`.
    reactor_log_entries: Arc<Mutex<Vec<(Uuid, String, LogEntry)>>>,
    /// Aggregate state after each fold:
    /// `(corr, position, event_id, aggregate_key, state_json)`.
    aggregate_state_snapshots:
        Arc<Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>>>,
    /// Reactor describe-DSL output per event:
    /// `(corr, position, event_id, reactor_id, description_json)`.
    reactor_description_snapshots:
        Arc<Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            global_log: Arc::new(Mutex::new(Vec::new())),
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

    /// Access the underlying global event log (for test assertions).
    pub fn global_log(&self) -> &Mutex<Vec<RecordedEvent>> {
        &self.global_log
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
        correlation_id: Uuid,
        attempt: u32,
        started_at: DateTime<Utc>,
    ) {
        self.reactor_executions.insert(
            (event_id, reactor_id.to_string()),
            (correlation_id, started_at, None, "running".to_string(), None, attempt as i32),
        );
    }

    fn reactor_completed(
        &self,
        event_id: Uuid,
        reactor_id: &str,
        correlation_id: Uuid,
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
            correlation_id,
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
        correlation_id: Uuid,
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
            correlation_id,
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

    fn reactor_dlq(
        &self,
        event_id: Uuid,
        reactor_id: &str,
        _correlation_id: Uuid,
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
        correlation_id: Uuid,
        position: LogCursor,
        event_id: Uuid,
        aggregate_key: &str,
        state: serde_json::Value,
    ) {
        self.aggregate_state_snapshots.lock().push((
            correlation_id,
            position.raw(),
            event_id,
            aggregate_key.to_string(),
            state,
        ));
    }

    fn reactor_description(
        &self,
        correlation_id: Uuid,
        position: LogCursor,
        event_id: Uuid,
        reactor_id: &str,
        description: serde_json::Value,
    ) {
        self.reactor_description_snapshots.lock().push((
            correlation_id,
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
        let events = log
            .iter()
            .filter(|e| e.position > after)
            .take(limit)
            .cloned()
            .collect();
        Ok(events)
    }

    async fn read_stream(
        &self,
        category: &str,
        stream_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        let log = self.global_log.lock();
        let events = log
            .iter()
            .filter(|e| {
                e.category == category
                    && e.stream_id == stream_id
                    && match after {
                        None => true,
                        Some(min) => e.revision > min,
                    }
            })
            .cloned()
            .collect();
        Ok(events)
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
        stream_id: Uuid,
        expected: crate::types::StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult> {
        use crate::types::StreamState;
        let Some(last_id) = events.last().map(|e| e.event_id) else {
            anyhow::bail!("append_to_stream: events must be non-empty");
        };
        let mut log = self.global_log.lock();

        // Idempotency: a batch is written atomically, so if the last
        // event_id is already present the whole batch is — return its
        // result regardless of expected state.
        if let Some(existing) = log.iter().find(|e| e.event_id == last_id) {
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
            && events.iter().any(|e| log.iter().any(|r| r.event_id == e.event_id))
        {
            anyhow::bail!(
                "append_to_stream: partial-overlap batch — an event_id already \
                 exists but the batch tail does not (event_ids must be all-new \
                 or all-already-persisted)"
            );
        }

        let count = log
            .iter()
            .filter(|e| e.category == category && e.stream_id == stream_id)
            .count() as u64;
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
            log.push(RecordedEvent {
                position,
                event_id: event.event_id,
                causation_id: event.causation_id,
                correlation_id: event.correlation_id,
                event_type: event.event_type,
                payload: event.payload,
                created_at: event.created_at,
                category: category.to_string(),
                stream_id,
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
        source_event_id: Uuid,
    ) -> Result<u32> {
        let key = (consumer_id.to_string(), source_event_id);
        let mut entry = self.reactor_attempts.entry(key).or_insert(0);
        *entry += 1;
        Ok(*entry)
    }

    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        source_event_id: Uuid,
    ) -> Result<()> {
        let key = (consumer_id.to_string(), source_event_id);
        self.reactor_attempts.remove(&key);
        Ok(())
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
