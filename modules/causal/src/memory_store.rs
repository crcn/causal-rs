//! In-memory backend: `EventLogBackend` + `CheckpointStore` +
//! `ReactorOutbox` + `SnapshotStore` + `ProjectionOps`.
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

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::checkpoint_store::{InsertableOutboxRow, OutboxRow, ReactorOutbox};
use crate::projection::{ProjectionFailure, ProjectionStatus};
use crate::reactor_observer::ReactorObserver;
use crate::types::*;

/// In-memory cursor row for one projection.
#[derive(Clone)]
struct ProjectionCursorEntry {
    cursor: LogCursor,
    paused: bool,
    last_error: Option<String>,
    last_attempt_at: Option<DateTime<Utc>>,
    consecutive_failures: u32,
}

/// In-memory backend implementing the v0.4 trait surface.
#[derive(Clone)]
pub struct MemoryStore {
    /// Global event log.
    global_log: Arc<Mutex<Vec<PersistedEvent>>>,
    /// Global position counter for event ordering.
    global_position: Arc<AtomicU64>,
    /// Snapshot store keyed by (aggregate_type, aggregate_id).
    snapshots: Arc<DashMap<(String, Uuid), Snapshot>>,
    /// Per-projection cursor + status.
    projection_cursors: Arc<DashMap<String, ProjectionCursorEntry>>,
    /// Per-projection DLQ rows. Idempotent on `(projection_id,
    /// event_id)` — matches the unique-constraint contract.
    projection_failures: Arc<Mutex<Vec<ProjectionFailure>>>,
    /// Reactor outbox rows pending drain to the log. Inserted by
    /// `commit_reactor_batch`, drained by `outbox_pending` /
    /// `outbox_delete`. Per C12, inserts here AND `set` on the cursor
    /// happen under the same Mutex lock for atomicity.
    outbox: Arc<Mutex<Vec<OutboxRow>>>,
    /// Monotonic id generator for outbox rows.
    next_outbox_id: Arc<AtomicI64>,
    /// DLQ attempt counter keyed by (consumer_id, source_event_id).
    /// Survives ReactorRunner reconstruction within the store's
    /// lifetime; lost on process crash (matches MemoryStore's
    /// "no durability" position).
    reactor_attempts: Arc<DashMap<(String, Uuid), u32>>,

    // ── Inspector observability (P13.a) ──────────────────────────
    //
    // These mirror what v0.3 MemoryStore captured via the legacy
    // IntentCommit path. Populated by `impl ReactorObserver for
    // MemoryStore`. Read by `causal_inspector` to render UI panes.
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
            projection_failures: Arc::new(Mutex::new(Vec::new())),
            outbox: Arc::new(Mutex::new(Vec::new())),
            next_outbox_id: Arc::new(AtomicI64::new(1)),
            reactor_attempts: Arc::new(DashMap::new()),
            reactor_executions: Arc::new(DashMap::new()),
            reactor_attempt_history: Arc::new(Mutex::new(Vec::new())),
            reactor_log_entries: Arc::new(Mutex::new(Vec::new())),
            aggregate_state_snapshots: Arc::new(Mutex::new(Vec::new())),
            reactor_description_snapshots: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Access the underlying global event log (for test assertions).
    pub fn global_log(&self) -> &Mutex<Vec<PersistedEvent>> {
        &self.global_log
    }

    // ── Inspector accessors ──────────────────────────────────────
    //
    // These return the same shape as v0.3 so the inspector reads
    // compile unchanged. Populated by the `ReactorObserver` impl
    // below as the engine calls hooks.

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

// ── EventLog implementation ─────────────────────────────────────────

#[async_trait]
impl crate::event_log::EventLogBackend for MemoryStore {
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

    /// Atomic CAS append for aggregate streams. Holds the global log
    /// mutex for the duration of the version check + insert so two
    /// concurrent callers can't both pass the check.
    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamVersion,
        event: NewEvent,
    ) -> Result<AppendResult> {
        let mut log = self.global_log.lock();

        // Idempotency: if event_id already exists, return existing
        // result regardless of expected_version. Matches the C1
        // contract: append is totally idempotent on event_id.
        if let Some(existing) = log.iter().find(|e| e.event_id == event.event_id) {
            return Ok(AppendResult {
                position: existing.position,
                version:  existing.version,
            });
        }

        let current_count = log
            .iter()
            .filter(|e| {
                e.aggregate_type.as_deref() == Some(aggregate_type)
                    && e.aggregate_id == Some(aggregate_id)
            })
            .count() as u64;
        let current = StreamVersion::from_raw(current_count);
        if current != expected {
            return Err(anyhow::Error::new(crate::event_log::ConflictError {
                expected,
                current,
            }));
        }

        let position = LogCursor::from_raw(self.global_position.fetch_add(1, Ordering::SeqCst));
        let new_version = StreamVersion::from_raw(current_count + 1);

        let persisted = PersistedEvent {
            position,
            event_id: event.event_id,
            parent_id: event.parent_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type,
            payload: event.payload,
            created_at: event.created_at,
            aggregate_type: Some(aggregate_type.to_string()),
            aggregate_id: Some(aggregate_id),
            version: Some(new_version),
            metadata: event.metadata,
            ephemeral: event.ephemeral,
            persistent: event.persistent,
        };

        log.push(persisted);

        Ok(AppendResult { position, version: Some(new_version) })
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
                slot.insert(ProjectionCursorEntry {
                    cursor: pos,
                    paused: false,
                    last_error: None,
                    last_attempt_at: None,
                    consecutive_failures: 0,
                });
            }
        }
        Ok(())
    }
}

// ── ReactorOutbox implementation (C12 atomicity) ────────────────────

// ── ProjectionOps (v0.4 ops surface) ────────────────────────────────

#[async_trait]
impl causal::projection::ProjectionOps for MemoryStore {
    async fn set_paused(&self, group_name: &str, paused: bool) -> Result<()> {
        use dashmap::mapref::entry::Entry;
        match self.projection_cursors.entry(group_name.to_string()) {
            Entry::Occupied(mut slot) => { slot.get_mut().paused = paused; }
            Entry::Vacant(slot) => {
                slot.insert(ProjectionCursorEntry {
                    cursor: LogCursor::ZERO,
                    paused,
                    last_error: None,
                    last_attempt_at: None,
                    consecutive_failures: 0,
                });
            }
        }
        Ok(())
    }

    async fn record_failure(
        &self,
        group_name: &str,
        event_id: Uuid,
        error: &str,
        attempts: u32,
    ) -> Result<()> {
        // Direct DLQ write. Idempotent on (group_name, event_id) —
        // matches the unique-constraint contract documented on the
        // trait. See trait docs re: atomicity (not bundled with
        // cursor advance under v0.4).
        let mut failures = self.projection_failures.lock();
        let already_present = failures.iter().any(|f| {
            f.projection_id == group_name && f.event_id == event_id
        });
        if !already_present {
            failures.push(ProjectionFailure {
                projection_id: group_name.to_string(),
                event_id,
                error: error.to_string(),
                attempts,
                failed_at: chrono::Utc::now(),
            });
        }
        Ok(())
    }

    async fn list_failures(
        &self,
        group_name: &str,
        limit: usize,
    ) -> Result<Vec<ProjectionFailure>> {
        let failures = self.projection_failures.lock();
        Ok(failures.iter()
            .rev()
            .filter(|f| f.projection_id == group_name)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn status(&self, group_name: &str) -> Result<Option<ProjectionStatus>> {
        Ok(self.projection_cursors.get(group_name).map(|e| ProjectionStatus {
            projection_id: group_name.to_string(),
            cursor: e.cursor,
            paused: e.paused,
            last_error: e.last_error.clone(),
            last_attempt_at: e.last_attempt_at,
            consecutive_failures: e.consecutive_failures,
        }))
    }

    async fn delete_failure(
        &self,
        group_name: &str,
        event_id: Uuid,
    ) -> Result<bool> {
        let mut failures = self.projection_failures.lock();
        let before = failures.len();
        failures.retain(|f| {
            !(f.projection_id == group_name && f.event_id == event_id)
        });
        Ok(failures.len() < before)
    }
}

// ── ReactorOutbox implementation (C12 atomicity) ────────────────────

#[async_trait]
impl ReactorOutbox for MemoryStore {
    async fn commit_reactor_batch(
        &self,
        rows: Vec<InsertableOutboxRow>,
        cursor: Option<(String, LogCursor)>,
    ) -> Result<()> {
        // Atomicity is via single Mutex lock spanning both writes.
        // Postgres equivalent is BEGIN; INSERT ...; UPDATE cursor; COMMIT.
        let mut outbox = self.outbox.lock();
        for row in rows {
            let assigned_id = self.next_outbox_id.fetch_add(1, Ordering::SeqCst);
            outbox.push(OutboxRow {
                id:              assigned_id,
                reactor_id:      row.reactor_id,
                source_event_id: row.source_event_id,
                output_index:    row.output_index,
                event_id:        row.event_id,
                event_type:      row.event_type,
                fact_payload:    row.fact_payload,
                correlation_id:  row.correlation_id,
                created_at:      Utc::now(),
            });
        }
        if let Some((consumer_id, pos)) = cursor {
            // Mirror the ProjectionStore semantics for cursor write —
            // upsert-with-create-on-missing.
            match self.projection_cursors.entry(consumer_id) {
                dashmap::mapref::entry::Entry::Vacant(slot) => {
                    slot.insert(ProjectionCursorEntry {
                        cursor: pos,
                        paused: false,
                        last_error: None,
                        last_attempt_at: None,
                        consecutive_failures: 0,
                    });
                }
                dashmap::mapref::entry::Entry::Occupied(mut slot) => {
                    let entry = slot.get_mut();
                    entry.cursor = pos;
                    entry.last_error = None;
                    entry.consecutive_failures = 0;
                }
            }
        }
        Ok(())
    }

    async fn outbox_pending(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        let outbox = self.outbox.lock();
        // Already FIFO by insertion (created_at ascending then id).
        Ok(outbox.iter().take(limit).cloned().collect())
    }

    async fn outbox_delete(&self, id: i64) -> Result<()> {
        let mut outbox = self.outbox.lock();
        outbox.retain(|r| r.id != id);
        Ok(())
    }

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
mod outbox_tests {
    use super::*;
    use crate::checkpoint_store::CheckpointStore;

    fn row(reactor_id: &str, idx: u32) -> InsertableOutboxRow {
        InsertableOutboxRow {
            reactor_id:      reactor_id.into(),
            source_event_id: Uuid::nil(),
            output_index:    idx,
            event_id:        Uuid::nil(),
            event_type:      "test.payload".into(),
            fact_payload:    serde_json::json!({"test": idx}),
            correlation_id:  Uuid::nil(),
        }
    }

    #[tokio::test]
    async fn commit_reactor_batch_inserts_rows_and_advances_cursor() {
        let store = MemoryStore::new();
        let pos = LogCursor::from_raw(42);

        store.commit_reactor_batch(
            vec![row("r1", 0), row("r1", 1), row("r1", 2)],
            Some(("r1".into(), pos)),
        ).await.unwrap();

        // Cursor advanced
        let cursor = CheckpointStore::get(&store, "r1").await.unwrap();
        assert_eq!(cursor, Some(pos));

        // Three rows pending
        let pending = store.outbox_pending(10).await.unwrap();
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].output_index, 0);
        assert_eq!(pending[1].output_index, 1);
        assert_eq!(pending[2].output_index, 2);

        // Backend assigned monotonic ids
        assert!(pending[0].id < pending[1].id);
        assert!(pending[1].id < pending[2].id);
    }

    #[tokio::test]
    async fn commit_reactor_batch_with_no_cursor_just_inserts_rows() {
        let store = MemoryStore::new();

        store.commit_reactor_batch(
            vec![row("r2", 0)],
            None,
        ).await.unwrap();

        assert!(CheckpointStore::get(&store, "r2").await.unwrap().is_none());
        assert_eq!(store.outbox_pending(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn outbox_pending_respects_limit() {
        let store = MemoryStore::new();
        let rows: Vec<InsertableOutboxRow> = (0..5).map(|i| row("r", i)).collect();
        store.commit_reactor_batch(rows, None).await.unwrap();

        let pending = store.outbox_pending(3).await.unwrap();
        assert_eq!(pending.len(), 3);
    }

    #[tokio::test]
    async fn outbox_pending_returns_oldest_first() {
        let store = MemoryStore::new();
        store.commit_reactor_batch(vec![row("r", 0)], None).await.unwrap();
        store.commit_reactor_batch(vec![row("r", 1)], None).await.unwrap();
        store.commit_reactor_batch(vec![row("r", 2)], None).await.unwrap();

        let pending = store.outbox_pending(10).await.unwrap();
        assert_eq!(pending[0].output_index, 0);
        assert_eq!(pending[1].output_index, 1);
        assert_eq!(pending[2].output_index, 2);
    }

    #[tokio::test]
    async fn outbox_delete_removes_specific_row() {
        let store = MemoryStore::new();
        store.commit_reactor_batch(
            vec![row("r", 0), row("r", 1), row("r", 2)],
            None,
        ).await.unwrap();

        let pending = store.outbox_pending(10).await.unwrap();
        let target_id = pending[1].id;

        store.outbox_delete(target_id).await.unwrap();

        let after = store.outbox_pending(10).await.unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].output_index, 0);
        assert_eq!(after[1].output_index, 2);
    }

    #[tokio::test]
    async fn outbox_delete_idempotent_on_missing_id() {
        // Per the trait contract, deleting an already-deleted id MUST
        // succeed — the relay may retry after a partial crash.
        let store = MemoryStore::new();
        store.outbox_delete(999_999).await.unwrap();
        store.outbox_delete(999_999).await.unwrap();
    }

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

    #[tokio::test]
    async fn commit_reactor_batch_clears_consumer_error_state() {
        use causal::checkpoint_store::CheckpointStore;
        use causal::projection::ProjectionOps;

        let store = MemoryStore::new();
        // Seed a cursor with error state. Use CheckpointStore::set to
        // establish the cursor row, then poke the error fields
        // directly (the v0.4 ProjectionOps surface doesn't expose
        // an in-flight error-state setter — `record_failure` writes
        // DLQ rows, not live consecutive-failure counters).
        CheckpointStore::set(&store, "r3", LogCursor::ZERO).await.unwrap();
        if let Some(mut entry) = store.projection_cursors.get_mut("r3") {
            entry.last_error = Some("prior failure".into());
            entry.consecutive_failures = 3;
        }

        let new_pos = LogCursor::from_raw(7);
        store.commit_reactor_batch(
            vec![row("r3", 0)],
            Some(("r3".into(), new_pos)),
        ).await.unwrap();

        let status = ProjectionOps::status(&store, "r3").await.unwrap().unwrap();
        assert_eq!(status.cursor, new_pos);
        assert!(status.last_error.is_none());
        assert_eq!(status.consecutive_failures, 0);
    }

    // ── P8: v0.4 ProjectionOps surface ─────────────────────────────

    #[tokio::test]
    async fn projection_ops_record_and_list_failures() {
        use causal::projection::ProjectionOps;
        let store = MemoryStore::new();
        let event_id = Uuid::new_v4();

        ProjectionOps::record_failure(&store, "p", event_id, "boom", 1).await.unwrap();
        ProjectionOps::record_failure(&store, "p", event_id, "boom retry", 2).await.unwrap();

        let failures = ProjectionOps::list_failures(&store, "p", 10).await.unwrap();
        assert_eq!(failures.len(), 1,
                   "idempotent on (group_name, event_id) — second call no-op");
        assert_eq!(failures[0].event_id, event_id);
        assert_eq!(failures[0].error, "boom");
        assert_eq!(failures[0].attempts, 1);

        let deleted = ProjectionOps::delete_failure(&store, "p", event_id).await.unwrap();
        assert!(deleted);
        assert!(ProjectionOps::list_failures(&store, "p", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn projection_ops_set_paused_and_status() {
        use causal::projection::ProjectionOps;
        let store = MemoryStore::new();

        // status() requires a cursor entry; seed one via CheckpointStore.
        CheckpointStore::set(&store, "p", LogCursor::from_raw(5)).await.unwrap();

        ProjectionOps::set_paused(&store, "p", true).await.unwrap();
        let status = ProjectionOps::status(&store, "p").await.unwrap().unwrap();
        assert!(status.paused);
        assert_eq!(status.cursor, LogCursor::from_raw(5));

        ProjectionOps::set_paused(&store, "p", false).await.unwrap();
        let status = ProjectionOps::status(&store, "p").await.unwrap().unwrap();
        assert!(!status.paused);
    }
}
