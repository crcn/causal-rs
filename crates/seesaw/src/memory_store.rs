//! In-memory queue store for seesaw Engine.
//!
//! Provides event and handler queues used by the Engine's built-in settle loop.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::Store;
use crate::types::*;

/// In-memory queue store for the Engine's settle loop.
///
/// Includes optional event persistence and snapshot support for testing
/// and simple use cases. The event log and snapshots are kept in-memory.
#[derive(Clone)]
pub struct MemoryStore {
    /// Event queue (FIFO per correlation_id)
    events: Arc<DashMap<Uuid, VecDeque<QueuedEvent>>>,
    /// Handler executions queue
    handler_queue: Arc<Mutex<VecDeque<QueuedHandler>>>,
    /// Completed handlers (for idempotency)
    completed_handlers: Arc<DashMap<(Uuid, String), serde_json::Value>>,
    /// In-flight handlers: populated by poll_next_handler, consumed by
    /// resolve_handler(Retry) to reconstruct the execution for re-enqueueing.
    in_flight: Arc<DashMap<(Uuid, String), QueuedHandler>>,
    /// Durable-ish join windows for same-batch fan-in (in-memory only).
    join_windows: Arc<Mutex<HashMap<(String, Uuid, Uuid), MemoryJoinWindow>>>,
    // ── Event persistence ────────────────────────────────────────
    /// Global event log for event persistence.
    global_log: Arc<Mutex<Vec<PersistedEvent>>>,
    /// Global position counter for event ordering.
    global_position: Arc<AtomicU64>,
    /// Snapshot store keyed by (aggregate_type, aggregate_id).
    snapshots: Arc<DashMap<(String, Uuid), Snapshot>>,
    /// Whether event persistence is enabled.
    persistence_enabled: bool,
    /// Cancelled correlation IDs with timestamp for TTL eviction.
    cancelled: Arc<DashMap<Uuid, Instant>>,
    /// TTL for cancelled entries (default 1 hour).
    cancel_ttl: std::time::Duration,
    /// Journal entries keyed by (handler_id, event_id).
    journal: Arc<DashMap<(String, Uuid), Vec<JournalEntry>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryJoinStatus {
    Open,
    Processing,
    Completed,
}

#[derive(Debug, Clone)]
struct MemoryJoinWindow {
    target_count: i32,
    status: MemoryJoinStatus,
    source_event_ids: HashSet<Uuid>,
    entries_by_index: HashMap<i32, JoinEntry>,
    expires_at: Option<DateTime<Utc>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            events: Arc::new(DashMap::new()),
            handler_queue: Arc::new(Mutex::new(VecDeque::new())),
            completed_handlers: Arc::new(DashMap::new()),
            in_flight: Arc::new(DashMap::new()),
            join_windows: Arc::new(Mutex::new(HashMap::new())),
            global_log: Arc::new(Mutex::new(Vec::new())),
            global_position: Arc::new(AtomicU64::new(1)),
            snapshots: Arc::new(DashMap::new()),
            persistence_enabled: false,
            cancelled: Arc::new(DashMap::new()),
            cancel_ttl: std::time::Duration::from_secs(3600),
            journal: Arc::new(DashMap::new()),
        }
    }

    /// Create a MemoryStore with event persistence and snapshots enabled.
    pub fn with_persistence() -> Self {
        Self {
            persistence_enabled: true,
            ..Self::new()
        }
    }

    /// Set the TTL for cancelled correlation entries.
    ///
    /// Entries older than this duration are lazily evicted on the next
    /// `is_cancelled` check. Defaults to 1 hour.
    pub fn with_cancel_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.cancel_ttl = ttl;
        self
    }

    /// Insert a queued handler directly (for test setup).
    pub async fn publish_handler_for_test(&self, handler: QueuedHandler) {
        self.handler_queue.lock().push_back(handler);
    }

    /// Access the global event log (for test assertions).
    pub fn global_log(&self) -> &Mutex<Vec<PersistedEvent>> {
        &self.global_log
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_handler_intent(
        &self,
        event_id: Uuid,
        handler_id: String,
        correlation_id: Uuid,
        event_type: String,
        event_payload: serde_json::Value,
        parent_event_id: Option<Uuid>,
        batch_id: Option<Uuid>,
        batch_index: Option<i32>,
        batch_size: Option<i32>,
        execute_at: DateTime<Utc>,
        timeout_seconds: i32,
        max_attempts: i32,
        priority: i32,
        hops: i32,
        join_window_timeout_seconds: Option<i32>,
        ephemeral: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Result<()> {
        let execution = QueuedHandler {
            event_id,
            handler_id,
            correlation_id,
            event_type,
            event_payload,
            parent_event_id,
            batch_id,
            batch_index,
            batch_size,
            execute_at,
            timeout_seconds,
            max_attempts,
            priority,
            hops,
            attempts: 0,
            join_window_timeout_seconds,
            ephemeral,
        };

        let mut queue = self.handler_queue.lock();
        queue.push_back(execution);
        Ok(())
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn publish(&self, event: QueuedEvent) -> Result<()> {
        let mut queue = self
            .events
            .entry(event.correlation_id)
            .or_insert_with(VecDeque::new);
        queue.push_back(event);
        Ok(())
    }

    async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        for mut entry in self.events.iter_mut() {
            if let Some(event) = entry.value_mut().pop_front() {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    async fn complete_event(&self, result: EventOutcome) -> Result<()> {
        match result {
            EventOutcome::Processed(commit) => {
                for intent in commit.queued_handler_intents {
                    self.insert_handler_intent(
                        commit.event_id,
                        intent.handler_id,
                        commit.correlation_id,
                        commit.event_type.clone(),
                        commit.event_payload.clone(),
                        intent.parent_event_id,
                        intent.batch_id,
                        intent.batch_index,
                        intent.batch_size,
                        intent.execute_at,
                        intent.timeout_seconds,
                        intent.max_attempts,
                        intent.priority,
                        intent.hops,
                        intent.join_window_timeout_seconds,
                        commit.ephemeral.clone(),
                    )
                    .await?;
                }

                for failure in commit.projection_failures {
                    self.resolve_handler(HandlerResolution::DeadLetter(HandlerDlq {
                        event_id: commit.event_id,
                        handler_id: failure.handler_id,
                        error: failure.error,
                        reason: failure.reason,
                        attempts: failure.attempts,
                        events_to_publish: Vec::new(),
                        log_entries: Vec::new(),
                    }))
                    .await?;
                }

                // ack is implicit for in-memory (event already dequeued)
            }
            EventOutcome::Rejected {
                event_id,
                error,
                reason,
                ..
            } => {
                // DLQ + ack in one step (ack is implicit for in-memory)
                eprintln!(
                    "Event rejected to DLQ: {} - {} ({})",
                    event_id, error, reason
                );
            }
        }
        Ok(())
    }

    async fn poll_next_handler(&self) -> Result<Option<QueuedHandler>> {
        let mut queue = self.handler_queue.lock();
        let now = Utc::now();
        // Find the ready handler with highest priority (lowest number)
        if let Some(pos) = queue
            .iter()
            .enumerate()
            .filter(|(_, e)| e.execute_at <= now)
            .min_by_key(|(_, e)| e.priority)
            .map(|(i, _)| i)
        {
            let execution = queue.remove(pos).unwrap();
            // Track in-flight so resolve_effect(Retry) can reconstruct for re-enqueueing
            self.in_flight.insert(
                (execution.event_id, execution.handler_id.clone()),
                execution.clone(),
            );
            Ok(Some(execution))
        } else {
            Ok(None)
        }
    }

    async fn earliest_pending_handler_at(&self) -> Result<Option<DateTime<Utc>>> {
        let queue = self.handler_queue.lock();
        Ok(queue.iter().map(|e| e.execute_at).min())
    }

    async fn resolve_handler(&self, resolution: HandlerResolution) -> Result<()> {
        match resolution {
            HandlerResolution::Complete(completion) => {
                self.in_flight
                    .remove(&(completion.event_id, completion.handler_id.clone()));
                self.clear_journal(&completion.handler_id, completion.event_id)
                    .await?;
                self.completed_handlers
                    .insert((completion.event_id, completion.handler_id), completion.result);
                for event in completion.events_to_publish {
                    self.publish(event).await?;
                }
            }
            HandlerResolution::Retry {
                event_id,
                handler_id,
                error,
                new_attempts,
                next_execute_at,
            } => {
                tracing::warn!(
                    "Handler retry: {}:{} - {} (attempt {})",
                    event_id,
                    handler_id,
                    error,
                    new_attempts
                );
                let key = (event_id, handler_id.clone());
                if let Some((_, mut execution)) = self.in_flight.remove(&key) {
                    execution.attempts = new_attempts;
                    execution.execute_at = next_execute_at;
                    let mut queue = self.handler_queue.lock();
                    queue.push_back(execution);
                } else {
                    tracing::warn!(
                        "resolve_handler(Retry): no in-flight execution found for {}:{} — retry will be lost",
                        key.0, handler_id
                    );
                }
            }
            HandlerResolution::DeadLetter(dlq) => {
                self.in_flight
                    .remove(&(dlq.event_id, dlq.handler_id.clone()));
                eprintln!(
                    "Handler sent to DLQ: {}:{} - {} (attempts: {})",
                    dlq.event_id, dlq.handler_id, dlq.error, dlq.attempts
                );
                for event in dlq.events_to_publish {
                    self.publish(event).await?;
                }
            }
        }
        Ok(())
    }

    async fn join_append_and_maybe_claim(
        &self,
        params: JoinAppendParams,
    ) -> Result<Option<Vec<JoinEntry>>> {
        let key = (
            params.join_handler_id,
            params.correlation_id,
            params.batch_id,
        );
        let mut windows = self.join_windows.lock();
        let window = windows.entry(key).or_insert_with(|| MemoryJoinWindow {
            target_count: params.batch_size,
            status: MemoryJoinStatus::Open,
            source_event_ids: HashSet::new(),
            entries_by_index: HashMap::new(),
            expires_at: params
                .join_window_timeout_seconds
                .map(|seconds| Utc::now() + Duration::seconds(seconds as i64)),
        });

        if window.status == MemoryJoinStatus::Completed {
            return Ok(None);
        }

        if window.target_count != params.batch_size {
            window.target_count = params.batch_size;
        }
        if window.expires_at.is_none() {
            window.expires_at = params
                .join_window_timeout_seconds
                .map(|seconds| Utc::now() + Duration::seconds(seconds as i64));
        }

        let already_seen_source = !window.source_event_ids.insert(params.source_event_id);
        if !already_seen_source {
            window
                .entries_by_index
                .entry(params.batch_index)
                .or_insert_with(|| JoinEntry {
                    source_event_id: params.source_event_id,
                    event_type: params.source_event_type,
                    payload: params.source_payload,
                    batch_id: params.batch_id,
                    batch_index: params.batch_index,
                    batch_size: params.batch_size,
                    created_at: params.source_created_at,
                });
        }

        let ready = window.entries_by_index.len() as i32 >= window.target_count;
        if ready && window.status == MemoryJoinStatus::Open {
            window.status = MemoryJoinStatus::Processing;
            let mut ordered = window
                .entries_by_index
                .values()
                .cloned()
                .collect::<Vec<_>>();
            ordered.sort_by_key(|entry| entry.batch_index);
            return Ok(Some(ordered));
        }

        Ok(None)
    }

    async fn join_complete(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
    ) -> Result<()> {
        let key = (join_handler_id, correlation_id, batch_id);
        if let Some(window) = self.join_windows.lock().get_mut(&key) {
            window.status = MemoryJoinStatus::Completed;
        }
        self.join_windows.lock().remove(&key);
        Ok(())
    }

    async fn join_release(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
        _error: String,
    ) -> Result<()> {
        let key = (join_handler_id, correlation_id, batch_id);
        if let Some(window) = self.join_windows.lock().get_mut(&key) {
            if window.status == MemoryJoinStatus::Processing {
                window.status = MemoryJoinStatus::Open;
            }
        }
        Ok(())
    }

    async fn expire_join_windows(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredJoinWindow>> {
        let mut windows = self.join_windows.lock();
        let mut expired = Vec::new();
        let mut expired_keys = Vec::new();

        for (key, window) in windows.iter() {
            if window.status != MemoryJoinStatus::Open {
                continue;
            }
            let Some(expires_at) = window.expires_at else {
                continue;
            };
            if expires_at > now {
                continue;
            }

            let mut source_event_ids =
                window.source_event_ids.iter().copied().collect::<Vec<_>>();
            source_event_ids.sort();
            expired.push(ExpiredJoinWindow {
                join_handler_id: key.0.clone(),
                correlation_id: key.1,
                batch_id: key.2,
                source_event_ids,
            });
            expired_keys.push(key.clone());
        }

        for key in expired_keys {
            windows.remove(&key);
        }

        Ok(expired)
    }

    // ── Event persistence overrides ──────────────────────────────

    async fn append_event(&self, event: NewEvent) -> Result<AppendResult> {
        if !self.persistence_enabled {
            return Ok(AppendResult { position: 0, version: None });
        }

        let mut log = self.global_log.lock();

        // Idempotency: if event_id already exists, return existing result
        if let Some(existing) = log.iter().find(|e| e.event_id == event.event_id) {
            return Ok(AppendResult {
                position: existing.position,
                version: existing.version,
            });
        }

        let position = self.global_position.fetch_add(1, Ordering::SeqCst);

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
            Some(count + 1)
        } else {
            None
        };

        log.push(PersistedEvent {
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
        });

        Ok(AppendResult { position, version })
    }

    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_version: Option<u64>,
    ) -> Result<Vec<PersistedEvent>> {
        let log = self.global_log.lock();
        let min_version = after_version.unwrap_or(0);
        let events = log
            .iter()
            .filter(|e| {
                e.aggregate_type.as_deref() == Some(aggregate_type)
                    && e.aggregate_id == Some(aggregate_id)
                    && (after_version.is_none() || e.version.unwrap_or(0) > min_version)
            })
            .cloned()
            .collect();
        Ok(events)
    }

    async fn load_global_from(
        &self,
        after_position: u64,
        limit: usize,
    ) -> Result<Vec<PersistedEvent>> {
        let log = self.global_log.lock();
        let events = log
            .iter()
            .filter(|e| e.position > after_position)
            .take(limit)
            .cloned()
            .collect();
        Ok(events)
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

    async fn cancel_correlation(&self, correlation_id: Uuid) -> Result<()> {
        self.cancelled.insert(correlation_id, Instant::now());
        Ok(())
    }

    async fn queue_status(&self, correlation_id: Uuid) -> Result<QueueStatus> {
        let pending_events = self
            .events
            .get(&correlation_id)
            .map(|q| q.len())
            .unwrap_or(0);

        let pending_handlers = self
            .handler_queue
            .lock()
            .iter()
            .filter(|e| e.correlation_id == correlation_id)
            .count();

        Ok(QueueStatus {
            pending_events,
            pending_handlers,
            dead_lettered: 0,
        })
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

    // ── Handler journaling ────────────────────────────────────────

    async fn load_journal(
        &self,
        handler_id: &str,
        event_id: Uuid,
    ) -> Result<Vec<JournalEntry>> {
        let key = (handler_id.to_string(), event_id);
        Ok(self.journal.get(&key).map(|v| v.clone()).unwrap_or_default())
    }

    async fn append_journal(
        &self,
        handler_id: &str,
        event_id: Uuid,
        seq: u32,
        value: serde_json::Value,
    ) -> Result<()> {
        let key = (handler_id.to_string(), event_id);
        self.journal
            .entry(key)
            .or_default()
            .push(JournalEntry { seq, value });
        Ok(())
    }

    async fn clear_journal(
        &self,
        handler_id: &str,
        event_id: Uuid,
    ) -> Result<()> {
        let key = (handler_id.to_string(), event_id);
        self.journal.remove(&key);
        Ok(())
    }
}
