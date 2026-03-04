//! In-memory queue store for seesaw Engine.
//!
//! Provides event and effect queues used by the Engine's built-in settle loop.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
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
    /// Effect executions queue
    effects: Arc<Mutex<VecDeque<QueuedHandlerExecution>>>,
    /// Completed effects (for idempotency)
    completed_effects: Arc<DashMap<(Uuid, String), serde_json::Value>>,
    /// In-flight effects: populated by poll_next_effect, consumed by
    /// fail_effect to reconstruct the execution for re-enqueueing.
    in_flight: Arc<DashMap<(Uuid, String), QueuedHandlerExecution>>,
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
            effects: Arc::new(Mutex::new(VecDeque::new())),
            completed_effects: Arc::new(DashMap::new()),
            in_flight: Arc::new(DashMap::new()),
            join_windows: Arc::new(Mutex::new(HashMap::new())),
            global_log: Arc::new(Mutex::new(Vec::new())),
            global_position: Arc::new(AtomicU64::new(1)),
            snapshots: Arc::new(DashMap::new()),
            persistence_enabled: false,
        }
    }

    /// Create a MemoryStore with event persistence and snapshots enabled.
    pub fn with_persistence() -> Self {
        Self {
            persistence_enabled: true,
            ..Self::new()
        }
    }

    /// Access the global event log (for test assertions).
    pub fn global_log(&self) -> &Mutex<Vec<PersistedEvent>> {
        &self.global_log
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_effect_intent(
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
    ) -> Result<()> {
        let execution = QueuedHandlerExecution {
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
        };

        let mut queue = self.effects.lock();
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

    async fn commit_event_processing(&self, commit: EventProcessingCommit) -> Result<()> {
        for intent in commit.queued_effect_intents {
            self.insert_effect_intent(
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
            )
            .await?;
        }

        for event in commit.emitted_events {
            self.publish(event).await?;
        }

        for failure in commit.inline_effect_failures {
            self.dlq_effect(EffectDlq {
                event_id: commit.event_id,
                handler_id: failure.handler_id,
                error: failure.error,
                reason: failure.reason,
                attempts: failure.attempts,
                events_to_publish: Vec::new(),
            })
            .await?;
        }

        // ack is implicit for in-memory (event already dequeued)
        Ok(())
    }

    async fn reject_event(
        &self,
        _event_row_id: i64,
        event_id: Uuid,
        error: String,
        reason: String,
    ) -> Result<()> {
        // DLQ + ack in one step (ack is implicit for in-memory)
        eprintln!(
            "Event rejected to DLQ: {} - {} ({})",
            event_id, error, reason
        );
        Ok(())
    }

    async fn poll_next_effect(&self) -> Result<Option<QueuedHandlerExecution>> {
        let mut queue = self.effects.lock();
        let now = Utc::now();
        if let Some(pos) = queue.iter().position(|e| e.execute_at <= now) {
            let execution = queue.remove(pos).unwrap();
            // Track in-flight so fail_effect can reconstruct for re-enqueueing
            self.in_flight.insert(
                (execution.event_id, execution.handler_id.clone()),
                execution.clone(),
            );
            Ok(Some(execution))
        } else {
            Ok(None)
        }
    }

    async fn earliest_pending_effect_at(&self) -> Result<Option<DateTime<Utc>>> {
        let queue = self.effects.lock();
        Ok(queue.iter().map(|e| e.execute_at).min())
    }

    async fn complete_effect(&self, completion: EffectCompletion) -> Result<()> {
        // Remove from in-flight tracking
        self.in_flight
            .remove(&(completion.event_id, completion.handler_id.clone()));

        self.completed_effects
            .insert((completion.event_id, completion.handler_id), completion.result);

        for event in completion.events_to_publish {
            self.publish(event).await?;
        }

        Ok(())
    }

    async fn fail_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        new_attempts: i32,
        next_execute_at: DateTime<Utc>,
    ) -> Result<()> {
        tracing::warn!(
            "Handler retry: {}:{} - {} (attempt {})",
            event_id,
            handler_id,
            error,
            new_attempts
        );

        // Reconstruct from in-flight tracking (populated by poll_next_effect)
        let key = (event_id, handler_id.clone());
        if let Some((_, mut execution)) = self.in_flight.remove(&key) {
            execution.attempts = new_attempts;
            execution.execute_at = next_execute_at;
            let mut queue = self.effects.lock();
            queue.push_back(execution);
        } else {
            tracing::warn!(
                "fail_effect: no in-flight execution found for {}:{} — retry will be lost",
                key.0, handler_id
            );
        }

        Ok(())
    }

    async fn dlq_effect(&self, dlq: EffectDlq) -> Result<()> {
        // Remove from in-flight tracking
        self.in_flight
            .remove(&(dlq.event_id, dlq.handler_id.clone()));

        eprintln!(
            "Handler sent to DLQ: {}:{} - {} (attempts: {})",
            dlq.event_id, dlq.handler_id, dlq.error, dlq.attempts
        );

        for event in dlq.events_to_publish {
            self.publish(event).await?;
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

    async fn append_event(&self, event: NewEvent) -> Result<u64> {
        if !self.persistence_enabled {
            return Ok(0);
        }

        let mut log = self.global_log.lock();

        // Idempotency: if event_id already exists, return existing position
        if let Some(existing) = log.iter().find(|e| e.event_id == event.event_id) {
            return Ok(existing.position);
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

        Ok(position)
    }

    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Result<Vec<PersistedEvent>> {
        let log = self.global_log.lock();
        let events = log
            .iter()
            .filter(|e| {
                e.aggregate_type.as_deref() == Some(aggregate_type)
                    && e.aggregate_id == Some(aggregate_id)
            })
            .cloned()
            .collect();
        Ok(events)
    }

    async fn load_stream_from(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_position: u64,
    ) -> Result<Vec<PersistedEvent>> {
        let log = self.global_log.lock();
        let events = log
            .iter()
            .filter(|e| {
                e.aggregate_type.as_deref() == Some(aggregate_type)
                    && e.aggregate_id == Some(aggregate_id)
                    && e.position > after_position
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
}
