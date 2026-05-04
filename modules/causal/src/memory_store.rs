//! In-memory EventLog + ReactorQueue for causal Engine.
//!
//! Provides event log persistence and reactor queue used by the Engine's settle loop.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use uuid::Uuid;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::event_log::EventLog;
use crate::projection::{
    ProjectionFailure, ProjectionStatus, ProjectionStore,
};
use crate::reactor_queue::ReactorQueue;
use crate::types::*;

/// In-memory cursor row for one async projection.
#[derive(Clone)]
struct ProjectionCursorEntry {
    cursor: LogCursor,
    paused: bool,
    last_error: Option<String>,
    last_attempt_at: Option<DateTime<Utc>>,
    consecutive_failures: u32,
}

/// In-memory EventLog + ReactorQueue for the Engine's settle loop.
///
/// Event log and snapshots are kept in-memory — suitable for testing
/// and simple single-process use cases.
#[derive(Clone)]
pub struct MemoryStore {
    /// Reactor executions queue
    reactor_queue: Arc<Mutex<VecDeque<QueuedReactor>>>,
    /// Completed reactors (for idempotency)
    completed_reactors: Arc<DashMap<(Uuid, String), serde_json::Value>>,
    /// In-flight reactors: populated by poll_next_handler, consumed by
    /// resolve_handler(Retry) to reconstruct the execution for re-enqueueing.
    in_flight: Arc<DashMap<(Uuid, String), QueuedReactor>>,
    // ── Event persistence ────────────────────────────────────────
    /// Global event log for event persistence.
    global_log: Arc<Mutex<Vec<PersistedEvent>>>,
    /// Global position counter for event ordering.
    global_position: Arc<AtomicU64>,
    /// Snapshot store keyed by (aggregate_type, aggregate_id).
    snapshots: Arc<DashMap<(String, Uuid), Snapshot>>,
    /// Cancelled correlation IDs with timestamp for TTL eviction.
    cancelled: Arc<DashMap<Uuid, Instant>>,
    /// TTL for cancelled entries (default 1 hour).
    cancel_ttl: std::time::Duration,
    /// Journal entries keyed by (reactor_id, event_id).
    journal: Arc<DashMap<(String, Uuid), Vec<JournalEntry>>>,
    /// Reactor gate descriptions keyed by correlation_id.
    reactor_descriptions: Arc<DashMap<Uuid, HashMap<String, serde_json::Value>>>,
    /// Reactor log entries keyed by (event_id, reactor_id).
    reactor_log_entries: Arc<Mutex<Vec<(Uuid, String, LogEntry)>>>,
    /// Per-event reactor description snapshots: (correlation_id, event_seq, event_id, reactor_id, description).
    reactor_description_snapshots: Arc<Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>>>,
    /// Per-event aggregate state snapshots: (correlation_id, event_seq, event_id, aggregate_key, state).
    aggregate_state_snapshots: Arc<Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>>>,
    /// Reactor execution timing: (event_id, reactor_id) → (correlation_id, started_at, completed_at, status, error, attempts).
    reactor_executions: Arc<DashMap<(Uuid, String), (Uuid, DateTime<Utc>, Option<DateTime<Utc>>, String, Option<String>, i32)>>,
    /// Per-attempt execution history: (event_id, reactor_id, correlation_id, attempt, status, error, started_at, completed_at).
    reactor_attempt_history: Arc<Mutex<Vec<(Uuid, String, Uuid, i32, String, Option<String>, DateTime<Utc>, DateTime<Utc>)>>>,
    /// Checkpoint position for ReactorQueue (last fully processed EventLog position).
    checkpoint: Arc<AtomicU64>,
    /// Optional broadcast channel for live event notifications.
    event_tx: Option<broadcast::Sender<PersistedEvent>>,
    /// Parked events recorded via `IntentCommit::park`. Each entry is
    /// `(event_id, reason)`. Cleared only by re-creating the store.
    parked: Arc<Mutex<Vec<(Uuid, String)>>>,
    /// Per-projection cursor + status for the `ProjectionStore` trait.
    projection_cursors: Arc<DashMap<String, ProjectionCursorEntry>>,
    /// Per-projection DLQ rows recorded by `advance_past_failure`.
    /// Idempotent on `(projection_id, event_id)` — duplicate calls
    /// are no-ops, matching the contract on backends with a unique
    /// constraint.
    projection_failures: Arc<Mutex<Vec<ProjectionFailure>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            reactor_queue: Arc::new(Mutex::new(VecDeque::new())),
            completed_reactors: Arc::new(DashMap::new()),
            in_flight: Arc::new(DashMap::new()),
            global_log: Arc::new(Mutex::new(Vec::new())),
            global_position: Arc::new(AtomicU64::new(1)),
            snapshots: Arc::new(DashMap::new()),
            cancelled: Arc::new(DashMap::new()),
            cancel_ttl: std::time::Duration::from_secs(3600),
            journal: Arc::new(DashMap::new()),
            reactor_descriptions: Arc::new(DashMap::new()),
            reactor_log_entries: Arc::new(Mutex::new(Vec::new())),
            reactor_description_snapshots: Arc::new(Mutex::new(Vec::new())),
            aggregate_state_snapshots: Arc::new(Mutex::new(Vec::new())),
            reactor_executions: Arc::new(DashMap::new()),
            reactor_attempt_history: Arc::new(Mutex::new(Vec::new())),
            checkpoint: Arc::new(AtomicU64::new(0)),
            event_tx: None,
            parked: Arc::new(Mutex::new(Vec::new())),
            projection_cursors: Arc::new(DashMap::new()),
            projection_failures: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Snapshot of events parked in this store via `IntentCommit::park`.
    ///
    /// Returns a `Vec<(event_id, reason)>` in park order. Useful for tests
    /// asserting that an event parked and inspecting the park reason. The
    /// underlying log is not cleared by reading.
    pub fn parked_events(&self) -> Vec<(Uuid, String)> {
        self.parked.lock().clone()
    }

    /// Create a MemoryStore with a broadcast channel for live event notifications.
    ///
    /// Returns the store and a receiver. Additional receivers can be obtained
    /// via [`subscribe()`](Self::subscribe).
    pub fn with_broadcast(capacity: usize) -> (Self, broadcast::Receiver<PersistedEvent>) {
        let (tx, rx) = broadcast::channel(capacity);
        let mut store = Self::new();
        store.event_tx = Some(tx);
        (store, rx)
    }

    /// Subscribe to live event notifications.
    ///
    /// Returns `None` if the store was created without a broadcast channel.
    pub fn subscribe(&self) -> Option<broadcast::Receiver<PersistedEvent>> {
        self.event_tx.as_ref().map(|tx| tx.subscribe())
    }

    /// Set the TTL for cancelled correlation entries.
    ///
    /// Entries older than this duration are lazily evicted on the next
    /// `is_cancelled` check. Defaults to 1 hour.
    pub fn with_cancel_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.cancel_ttl = ttl;
        self
    }

    /// Insert a queued reactor directly (for test setup).
    pub async fn publish_reactor_for_test(&self, reactor: QueuedReactor) {
        self.reactor_queue.lock().push_back(reactor);
    }

    /// Set the checkpoint position directly (for test setup / resume simulation).
    pub fn set_checkpoint(&self, position: LogCursor) {
        self.checkpoint.store(position.raw(), Ordering::SeqCst);
    }

    /// Access the global event log (for test assertions).
    /// Access the stored reactor log entries.
    pub fn reactor_log_entries(&self) -> &Mutex<Vec<(Uuid, String, LogEntry)>> {
        &self.reactor_log_entries
    }

    pub fn global_log(&self) -> &Mutex<Vec<PersistedEvent>> {
        &self.global_log
    }

    /// Access per-event reactor description snapshots.
    pub fn reactor_description_snapshots(&self) -> &Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>> {
        &self.reactor_description_snapshots
    }

    /// Access per-event aggregate state snapshots.
    pub fn aggregate_state_snapshots(&self) -> &Mutex<Vec<(Uuid, u64, Uuid, String, serde_json::Value)>> {
        &self.aggregate_state_snapshots
    }

    /// Access reactor execution timing records.
    pub fn reactor_executions(&self) -> &DashMap<(Uuid, String), (Uuid, DateTime<Utc>, Option<DateTime<Utc>>, String, Option<String>, i32)> {
        &self.reactor_executions
    }

    /// Access the per-attempt execution history.
    pub fn reactor_attempt_history(&self) -> &Mutex<Vec<(Uuid, String, Uuid, i32, String, Option<String>, DateTime<Utc>, DateTime<Utc>)>> {
        &self.reactor_attempt_history
    }

    /// Internal journal clear (avoids trait method ambiguity).
    fn clear_journal_internal(&self, reactor_id: &str, event_id: Uuid) {
        let key = (reactor_id.to_string(), event_id);
        self.journal.remove(&key);
    }

}

// ── EventLog implementation ─────────────────────────────────────────

#[async_trait]
impl EventLog for MemoryStore {
    async fn append(&self, event: NewEvent) -> Result<AppendResult> {
        let mut log = self.global_log.lock();

        // Idempotency: if event_id already exists, return existing result
        if let Some(existing) = log.iter().find(|e| e.event_id == event.event_id) {
            return Ok(AppendResult {
                position: existing.position,
                version: existing.version,
            });
        }

        let position = LogCursor::from_raw(self.global_position.fetch_add(1, Ordering::SeqCst));

        // Compute per-aggregate version if aggregate metadata is present
        let version = if let (Some(ref agg_type), Some(agg_id)) =
            (&event.aggregate_type, event.aggregate_id)
        {
            let count = log
                .iter()
                .filter(|e| {
                    e.aggregate_type.as_deref() == Some(agg_type)
                        && e.aggregate_id == Some(agg_id)
                })
                .count() as u64;
            Some(StreamVersion::from_raw(count + 1))
        } else {
            None
        };

        let persisted = PersistedEvent {
            position,
            event_id: event.event_id,
            parent_id: event.parent_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type,
            payload: event.payload,
            created_at: event.created_at,
            aggregate_type: event.aggregate_type,
            aggregate_id: event.aggregate_id,
            version,
            metadata: event.metadata,
            ephemeral: event.ephemeral,
            persistent: event.persistent,
        };

        // Broadcast before pushing (clone is cheap — Arc for ephemeral)
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(persisted.clone());
        }

        log.push(persisted);

        Ok(AppendResult { position, version })
    }

    async fn load_from(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<PersistedEvent>> {
        let log = self.global_log.lock();
        let events = log
            .iter()
            .filter(|e| e.position > after)
            .take(limit)
            .cloned()
            .collect();
        Ok(events)
    }

    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_version: Option<StreamVersion>,
    ) -> Result<Vec<PersistedEvent>> {
        let log = self.global_log.lock();
        let min_version = after_version.unwrap_or(StreamVersion::ZERO);
        let events = log
            .iter()
            .filter(|e| {
                e.aggregate_type.as_deref() == Some(aggregate_type)
                    && e.aggregate_id == Some(aggregate_id)
                    && (after_version.is_none() || e.version.unwrap_or(StreamVersion::ZERO) > min_version)
            })
            .cloned()
            .collect();
        Ok(events)
    }

    async fn latest_position(&self) -> Result<LogCursor> {
        let log = self.global_log.lock();
        Ok(log.last().map(|e| e.position).unwrap_or(LogCursor::ZERO))
    }

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
}

// ── ReactorQueue implementation ─────────────────────────────────────

#[async_trait]
impl ReactorQueue for MemoryStore {
    async fn enqueue(&self, commit: IntentCommit) -> Result<()> {
        // Persist reactor descriptions atomically
        if !commit.reactor_descriptions.is_empty() {
            // Also store per-event snapshots for the aggregate state timeline
            let event_seq = commit.checkpoint.raw();
            {
                let mut snapshots = self.reactor_description_snapshots.lock();
                for (reactor_id, description) in &commit.reactor_descriptions {
                    snapshots.push((
                        commit.correlation_id,
                        event_seq,
                        commit.event_id,
                        reactor_id.clone(),
                        description.clone(),
                    ));
                }
            }

            let mut entry = self
                .reactor_descriptions
                .entry(commit.correlation_id)
                .or_default();
            entry.extend(commit.reactor_descriptions);
        }

        // Persist aggregate state snapshots
        if !commit.aggregate_snapshots.is_empty() {
            let event_seq = commit.checkpoint.raw();
            let mut snapshots = self.aggregate_state_snapshots.lock();
            for (aggregate_key, state) in &commit.aggregate_snapshots {
                snapshots.push((
                    commit.correlation_id,
                    event_seq,
                    commit.event_id,
                    aggregate_key.clone(),
                    state.clone(),
                ));
            }
        }

        // Handle park (DLQ for events): record for programmatic inspection
        // via `parked_events()`, plus a trace log for visibility.
        if let Some(park) = &commit.park {
            tracing::warn!(
                event_id = %commit.event_id,
                reason = %park.reason,
                "event parked"
            );
            self.parked.lock().push((commit.event_id, park.reason.clone()));
        }

        // Create reactor intents
        for intent in commit.intents {
            let execution = QueuedReactor {
                event_id: commit.event_id,
                reactor_id: intent.reactor_id,
                correlation_id: commit.correlation_id,
                event_type: commit.event_type.clone(),
                event_payload: commit.event_payload.clone(),
                parent_event_id: intent.parent_event_id,
                execute_at: intent.execute_at,
                timeout_seconds: intent.timeout_seconds,
                max_attempts: intent.max_attempts,
                priority: intent.priority,
                hops: intent.hops,
                attempts: 0,
                ephemeral: None, // Engine injects ephemeral from its cache
            };
            self.reactor_queue.lock().push_back(execution);
        }

        // Advance checkpoint
        self.checkpoint.store(commit.checkpoint.raw(), Ordering::SeqCst);

        Ok(())
    }

    async fn checkpoint(&self) -> Result<LogCursor> {
        Ok(LogCursor::from_raw(self.checkpoint.load(Ordering::SeqCst)))
    }

    async fn dequeue(&self) -> Result<Option<QueuedReactor>> {
        let mut queue = self.reactor_queue.lock();
        let now = Utc::now();
        if let Some(pos) = queue
            .iter()
            .enumerate()
            .filter(|(_, e)| e.execute_at <= now)
            .min_by_key(|(_, e)| e.priority)
            .map(|(i, _)| i)
        {
            let execution = queue.remove(pos).unwrap();
            self.in_flight.insert(
                (execution.event_id, execution.reactor_id.clone()),
                execution.clone(),
            );
            // Record execution start time
            self.reactor_executions.insert(
                (execution.event_id, execution.reactor_id.clone()),
                (execution.correlation_id, Utc::now(), None, "running".to_string(), None, execution.attempts),
            );
            Ok(Some(execution))
        } else {
            Ok(None)
        }
    }

    async fn earliest_pending_at(&self) -> Result<Option<DateTime<Utc>>> {
        let queue = self.reactor_queue.lock();
        Ok(queue.iter().map(|e| e.execute_at).min())
    }

    async fn resolve(&self, resolution: ReactorResolution) -> Result<()> {
        match resolution {
            ReactorResolution::Complete(completion) => {
                self.in_flight
                    .remove(&(completion.event_id, completion.reactor_id.clone()));
                self.clear_journal_internal(&completion.reactor_id, completion.event_id);
                // Record attempt history before updating execution record
                let now = Utc::now();
                if let Some(entry) = self.reactor_executions.get(&(completion.event_id, completion.reactor_id.clone())) {
                    let (corr_id, started_at, _, _, _, attempts) = entry.value();
                    self.reactor_attempt_history.lock().push((
                        completion.event_id,
                        completion.reactor_id.clone(),
                        *corr_id,
                        *attempts,
                        "completed".to_string(),
                        None,
                        *started_at,
                        now,
                    ));
                }
                // Record completion time
                if let Some(mut entry) = self.reactor_executions.get_mut(&(completion.event_id, completion.reactor_id.clone())) {
                    entry.2 = Some(now);
                    entry.3 = "completed".to_string();
                }
                // Store log entries for inspector
                if !completion.log_entries.is_empty() {
                    let mut logs = self.reactor_log_entries.lock();
                    for entry in &completion.log_entries {
                        logs.push((
                            completion.event_id,
                            completion.reactor_id.clone(),
                            entry.clone(),
                        ));
                    }
                }
                self.completed_reactors
                    .insert((completion.event_id, completion.reactor_id), completion.result);
            }
            ReactorResolution::Retry {
                event_id,
                reactor_id,
                error,
                new_attempts,
                next_execute_at,
                log_entries,
            } => {
                tracing::warn!(
                    "Reactor retry: {}:{} - {} (attempt {})",
                    event_id,
                    reactor_id,
                    error,
                    new_attempts
                );
                // Record attempt history before updating execution record
                let now = Utc::now();
                if let Some(entry) = self.reactor_executions.get(&(event_id, reactor_id.clone())) {
                    let (corr_id, started_at, _, _, _, _) = entry.value();
                    self.reactor_attempt_history.lock().push((
                        event_id,
                        reactor_id.clone(),
                        *corr_id,
                        new_attempts - 1, // this attempt number (0-based)
                        "retry".to_string(),
                        Some(error.clone()),
                        *started_at,
                        now,
                    ));
                }
                // Update attempt count in execution record
                if let Some(mut entry) = self.reactor_executions.get_mut(&(event_id, reactor_id.clone())) {
                    entry.5 = new_attempts;
                }
                // Store log entries for inspector
                if !log_entries.is_empty() {
                    let mut logs = self.reactor_log_entries.lock();
                    for entry in &log_entries {
                        logs.push((
                            event_id,
                            reactor_id.clone(),
                            entry.clone(),
                        ));
                    }
                }
                let key = (event_id, reactor_id.clone());
                if let Some((_, mut execution)) = self.in_flight.remove(&key) {
                    execution.attempts = new_attempts;
                    execution.execute_at = next_execute_at;
                    self.reactor_queue.lock().push_back(execution);
                } else {
                    tracing::warn!(
                        "resolve(Retry): no in-flight execution found for {}:{} — retry will be lost",
                        key.0, reactor_id
                    );
                }
            }
            ReactorResolution::DeadLetter(dlq) => {
                self.in_flight
                    .remove(&(dlq.event_id, dlq.reactor_id.clone()));
                // Record attempt history before updating execution record
                let now = Utc::now();
                if let Some(entry) = self.reactor_executions.get(&(dlq.event_id, dlq.reactor_id.clone())) {
                    let (corr_id, started_at, _, _, _, _) = entry.value();
                    self.reactor_attempt_history.lock().push((
                        dlq.event_id,
                        dlq.reactor_id.clone(),
                        *corr_id,
                        dlq.attempts,
                        "error".to_string(),
                        Some(dlq.error.clone()),
                        *started_at,
                        now,
                    ));
                }
                // Record failure
                if let Some(mut entry) = self.reactor_executions.get_mut(&(dlq.event_id, dlq.reactor_id.clone())) {
                    entry.2 = Some(now);
                    entry.3 = "error".to_string();
                    entry.4 = Some(dlq.error.clone());
                    entry.5 = dlq.attempts;
                }
                // Store log entries for inspector (same as Complete path)
                if !dlq.log_entries.is_empty() {
                    let mut logs = self.reactor_log_entries.lock();
                    for entry in &dlq.log_entries {
                        logs.push((
                            dlq.event_id,
                            dlq.reactor_id.clone(),
                            entry.clone(),
                        ));
                    }
                }
                eprintln!(
                    "Reactor sent to DLQ: {}:{} - {} (attempts: {})",
                    dlq.event_id, dlq.reactor_id, dlq.error, dlq.attempts
                );
            }
        }
        Ok(())
    }

    // ── Journaling ────────────────────────────────────────────────────

    async fn load_journal(
        &self,
        reactor_id: &str,
        event_id: Uuid,
    ) -> Result<Vec<JournalEntry>> {
        let key = (reactor_id.to_string(), event_id);
        Ok(self.journal.get(&key).map(|v| v.clone()).unwrap_or_default())
    }

    async fn append_journal(
        &self,
        reactor_id: &str,
        event_id: Uuid,
        seq: u32,
        value: serde_json::Value,
    ) -> Result<()> {
        let key = (reactor_id.to_string(), event_id);
        self.journal
            .entry(key)
            .or_default()
            .push(JournalEntry { seq, value });
        Ok(())
    }

    async fn clear_journal(
        &self,
        reactor_id: &str,
        event_id: Uuid,
    ) -> Result<()> {
        let key = (reactor_id.to_string(), event_id);
        self.journal.remove(&key);
        Ok(())
    }

    // ── Coordination ──────────────────────────────────────────────────

    async fn cancel(&self, correlation_id: Uuid) -> Result<()> {
        self.cancelled.insert(correlation_id, Instant::now());
        Ok(())
    }

    async fn is_cancelled(&self, correlation_id: Uuid) -> Result<bool> {
        match self.cancelled.get(&correlation_id) {
            Some(entry) => {
                if entry.value().elapsed() > self.cancel_ttl {
                    drop(entry);
                    self.cancelled.remove(&correlation_id);
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            None => Ok(false),
        }
    }

    async fn status(&self, correlation_id: Uuid) -> Result<QueueStatus> {
        let pending_reactors = self
            .reactor_queue
            .lock()
            .iter()
            .filter(|e| e.correlation_id == correlation_id)
            .count();

        Ok(QueueStatus {
            pending_reactors,
            dead_lettered: 0,
        })
    }

    async fn set_descriptions(
        &self,
        correlation_id: Uuid,
        descriptions: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        let mut entry = self.reactor_descriptions.entry(correlation_id).or_default();
        entry.extend(descriptions);
        Ok(())
    }

    async fn get_descriptions(
        &self,
        correlation_id: Uuid,
    ) -> Result<HashMap<String, serde_json::Value>> {
        Ok(self
            .reactor_descriptions
            .get(&correlation_id)
            .map(|e| e.value().clone())
            .unwrap_or_default())
    }
}

#[async_trait]
impl ProjectionStore for MemoryStore {
    async fn init_projection_cursor(
        &self,
        projection_id: &str,
        start: LogCursor,
    ) -> Result<bool> {
        use dashmap::mapref::entry::Entry;
        match self.projection_cursors.entry(projection_id.to_string()) {
            Entry::Occupied(_) => Ok(false),
            Entry::Vacant(slot) => {
                slot.insert(ProjectionCursorEntry {
                    cursor: start,
                    paused: false,
                    last_error: None,
                    last_attempt_at: None,
                    consecutive_failures: 0,
                });
                Ok(true)
            }
        }
    }

    async fn get_projection_cursor(
        &self,
        projection_id: &str,
    ) -> Result<Option<LogCursor>> {
        Ok(self
            .projection_cursors
            .get(projection_id)
            .map(|e| e.cursor))
    }

    async fn advance_projection_cursor(
        &self,
        projection_id: &str,
        expected_from: LogCursor,
        to: LogCursor,
    ) -> Result<bool> {
        let Some(mut entry) = self.projection_cursors.get_mut(projection_id) else {
            return Ok(false);
        };
        if entry.cursor != expected_from {
            return Ok(false);
        }
        entry.cursor = to;
        entry.last_error = None;
        entry.consecutive_failures = 0;
        Ok(true)
    }

    async fn advance_past_failure(
        &self,
        projection_id: &str,
        expected_from: LogCursor,
        to: LogCursor,
        event_id: Uuid,
        error: &str,
        attempts: u32,
    ) -> Result<bool> {
        // Acquire cursor entry first (per-entry lock), then DLQ vec lock.
        // Order is consistent across all callers — this is the only method
        // that takes both locks, so no deadlock window.
        let Some(mut entry) = self.projection_cursors.get_mut(projection_id) else {
            return Ok(false);
        };
        if entry.cursor != expected_from {
            return Ok(false);
        }

        // Idempotent DLQ write on (projection_id, event_id), matching the
        // primary-key contract documented on the trait.
        let mut failures = self.projection_failures.lock();
        let already_present = failures.iter().any(|f| {
            f.projection_id == projection_id && f.event_id == event_id
        });
        if !already_present {
            failures.push(ProjectionFailure {
                projection_id: projection_id.to_string(),
                event_id,
                error: error.to_string(),
                attempts,
                failed_at: Utc::now(),
            });
        }
        drop(failures);

        // Atomic with the DLQ write because both succeed before this method
        // returns and a panic between them is impossible (no .await points).
        entry.cursor = to;
        entry.last_error = None;
        entry.consecutive_failures = 0;
        Ok(true)
    }

    async fn set_projection_error(
        &self,
        projection_id: &str,
        error: Option<&str>,
        consecutive_failures: u32,
    ) -> Result<()> {
        if let Some(mut entry) = self.projection_cursors.get_mut(projection_id) {
            entry.last_error = error.map(|s| s.to_string());
            entry.consecutive_failures = consecutive_failures;
            entry.last_attempt_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn projection_status(
        &self,
        projection_id: &str,
    ) -> Result<Option<ProjectionStatus>> {
        Ok(self.projection_cursors.get(projection_id).map(|e| {
            ProjectionStatus {
                projection_id: projection_id.to_string(),
                cursor: e.cursor,
                paused: e.paused,
                last_error: e.last_error.clone(),
                last_attempt_at: e.last_attempt_at,
                consecutive_failures: e.consecutive_failures,
            }
        }))
    }

    async fn list_projections(&self) -> Result<Vec<ProjectionStatus>> {
        Ok(self
            .projection_cursors
            .iter()
            .map(|entry| ProjectionStatus {
                projection_id: entry.key().clone(),
                cursor: entry.cursor,
                paused: entry.paused,
                last_error: entry.last_error.clone(),
                last_attempt_at: entry.last_attempt_at,
                consecutive_failures: entry.consecutive_failures,
            })
            .collect())
    }

    async fn set_projection_paused(
        &self,
        projection_id: &str,
        paused: bool,
    ) -> Result<()> {
        if let Some(mut entry) = self.projection_cursors.get_mut(projection_id) {
            entry.paused = paused;
        }
        Ok(())
    }

    async fn reset_projection(
        &self,
        projection_id: &str,
        to: LogCursor,
    ) -> Result<()> {
        if let Some(mut entry) = self.projection_cursors.get_mut(projection_id) {
            entry.cursor = to;
            entry.last_error = None;
            entry.consecutive_failures = 0;
            entry.last_attempt_at = None;
        }
        Ok(())
    }

    async fn delete_projection(&self, projection_id: &str) -> Result<()> {
        self.projection_cursors.remove(projection_id);
        let mut failures = self.projection_failures.lock();
        failures.retain(|f| f.projection_id != projection_id);
        Ok(())
    }

    async fn list_projection_failures(
        &self,
        projection_id: &str,
        limit: usize,
    ) -> Result<Vec<ProjectionFailure>> {
        let failures = self.projection_failures.lock();
        let mut matching: Vec<_> = failures
            .iter()
            .filter(|f| f.projection_id == projection_id)
            .cloned()
            .collect();
        // Most-recent first
        matching.sort_by(|a, b| b.failed_at.cmp(&a.failed_at));
        matching.truncate(limit);
        Ok(matching)
    }

    async fn delete_projection_failure(
        &self,
        projection_id: &str,
        event_id: Uuid,
    ) -> Result<bool> {
        let mut failures = self.projection_failures.lock();
        let before = failures.len();
        failures.retain(|f| {
            !(f.projection_id == projection_id && f.event_id == event_id)
        });
        Ok(failures.len() < before)
    }
}
