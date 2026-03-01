//! In-memory queue store for seesaw Engine.
//!
//! Provides event and effect queues used by the Engine's built-in settle loop.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

use crate::types::*;

/// In-memory queue store for the Engine's settle loop.
#[derive(Clone)]
pub(crate) struct MemoryStore {
    /// Event queue (FIFO per correlation_id)
    events: Arc<DashMap<Uuid, VecDeque<QueuedEvent>>>,
    /// Global event sequence for IDs
    event_seq: Arc<AtomicI64>,
    /// Effect executions queue
    effects: Arc<Mutex<VecDeque<QueuedHandlerExecution>>>,
    /// Completed effects (for idempotency)
    completed_effects: Arc<DashMap<(Uuid, String), serde_json::Value>>,
    /// Durable-ish join windows for same-batch fan-in (in-memory only).
    join_windows: Arc<Mutex<HashMap<(String, Uuid, Uuid), MemoryJoinWindow>>>,
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
            event_seq: Arc::new(AtomicI64::new(1)),
            effects: Arc::new(Mutex::new(VecDeque::new())),
            completed_effects: Arc::new(DashMap::new()),
            join_windows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn publish(&self, event: QueuedEvent) -> Result<()> {
        let mut queue = self
            .events
            .entry(event.correlation_id)
            .or_insert_with(VecDeque::new);
        queue.push_back(event);
        Ok(())
    }

    pub async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        for mut entry in self.events.iter_mut() {
            if let Some(event) = entry.value_mut().pop_front() {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    pub async fn ack(&self, _id: i64) -> Result<()> {
        Ok(())
    }

    pub async fn commit_event_processing(&self, commit: EventProcessingCommit) -> Result<()> {
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
            self.dlq_effect(
                commit.event_id,
                failure.handler_id,
                failure.error,
                failure.reason,
                failure.attempts,
            )
            .await?;
        }

        self.ack(commit.event_row_id).await?;
        Ok(())
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

    pub async fn poll_next_effect(&self) -> Result<Option<QueuedHandlerExecution>> {
        let mut queue = self.effects.lock();
        let now = Utc::now();
        if let Some(pos) = queue.iter().position(|e| e.execute_at <= now) {
            Ok(queue.remove(pos))
        } else {
            Ok(None)
        }
    }

    /// Returns the earliest `execute_at` of any queued effect, if any exist.
    ///
    /// Used by the settle loop to sleep until the next effect becomes ready
    /// instead of exiting prematurely when only future-dated effects remain.
    pub fn earliest_pending_effect_at(&self) -> Option<DateTime<Utc>> {
        let queue = self.effects.lock();
        queue.iter().map(|e| e.execute_at).min()
    }

    pub async fn complete_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        result: serde_json::Value,
    ) -> Result<()> {
        self.completed_effects
            .insert((event_id, handler_id), result);
        Ok(())
    }

    pub async fn complete_effect_with_events(
        &self,
        event_id: Uuid,
        handler_id: String,
        result: serde_json::Value,
        emitted_events: Vec<EmittedEvent>,
        correlation_id: Uuid,
        parent_hops: i32,
    ) -> Result<()> {
        self.complete_effect(event_id, handler_id.clone(), result)
            .await?;

        for (emitted_index, emitted) in emitted_events.into_iter().enumerate() {
            let new_event_id = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!(
                    "{}-{}-{}-{}",
                    event_id, handler_id, emitted.event_type, emitted_index
                )
                .as_bytes(),
            );

            let queued = QueuedEvent {
                id: self.event_seq.fetch_add(1, Ordering::SeqCst),
                event_id: new_event_id,
                parent_id: Some(event_id),
                correlation_id,
                event_type: emitted.event_type,
                payload: emitted.payload,
                hops: parent_hops + 1,
                retry_count: 0,
                batch_id: emitted.batch_id,
                batch_index: emitted.batch_index,
                batch_size: emitted.batch_size,
                created_at: Utc::now(),
            };

            self.publish(queued).await?;
        }

        Ok(())
    }

    pub async fn fail_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        attempts: i32,
        execution: QueuedHandlerExecution,
    ) -> Result<()> {
        tracing::warn!(
            "Handler retry: {}:{} - {} (attempt {})",
            event_id,
            handler_id,
            error,
            attempts
        );
        let mut retried = execution;
        retried.attempts = attempts + 1;
        let mut queue = self.effects.lock();
        queue.push_back(retried);
        Ok(())
    }

    pub async fn dlq_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        _reason: String,
        attempts: i32,
    ) -> Result<()> {
        eprintln!(
            "Handler sent to DLQ: {}:{} - {} (attempts: {})",
            event_id, handler_id, error, attempts
        );
        Ok(())
    }

    pub async fn dlq_effect_with_events(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        reason: String,
        attempts: i32,
        emitted_events: Vec<EmittedEvent>,
        correlation_id: Uuid,
        parent_hops: i32,
    ) -> Result<()> {
        self.dlq_effect(event_id, handler_id.clone(), error, reason, attempts)
            .await?;

        // Publish terminal events from on_failure mapper
        for (emitted_index, emitted) in emitted_events.into_iter().enumerate() {
            let new_event_id = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!(
                    "{}-{}-dlq-{}-{}",
                    event_id, handler_id, emitted.event_type, emitted_index
                )
                .as_bytes(),
            );

            let queued = QueuedEvent {
                id: self.event_seq.fetch_add(1, Ordering::SeqCst),
                event_id: new_event_id,
                parent_id: Some(event_id),
                correlation_id,
                event_type: emitted.event_type,
                payload: emitted.payload,
                hops: parent_hops + 1,
                retry_count: 0,
                batch_id: emitted.batch_id,
                batch_index: emitted.batch_index,
                batch_size: emitted.batch_size,
                created_at: Utc::now(),
            };

            self.publish(queued).await?;
        }

        Ok(())
    }

    pub async fn join_same_batch_append_and_maybe_claim(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        source_event_id: Uuid,
        source_event_type: String,
        source_payload: serde_json::Value,
        source_created_at: DateTime<Utc>,
        batch_id: Uuid,
        batch_index: i32,
        batch_size: i32,
        join_window_timeout_seconds: Option<i32>,
    ) -> Result<Option<Vec<JoinEntry>>> {
        let key = (join_handler_id, correlation_id, batch_id);
        let mut windows = self.join_windows.lock();
        let window = windows.entry(key).or_insert_with(|| MemoryJoinWindow {
            target_count: batch_size,
            status: MemoryJoinStatus::Open,
            source_event_ids: HashSet::new(),
            entries_by_index: HashMap::new(),
            expires_at: join_window_timeout_seconds
                .map(|seconds| Utc::now() + Duration::seconds(seconds as i64)),
        });

        if window.status == MemoryJoinStatus::Completed {
            return Ok(None);
        }

        if window.target_count != batch_size {
            window.target_count = batch_size;
        }
        if window.expires_at.is_none() {
            window.expires_at = join_window_timeout_seconds
                .map(|seconds| Utc::now() + Duration::seconds(seconds as i64));
        }

        let already_seen_source = !window.source_event_ids.insert(source_event_id);
        if !already_seen_source {
            window
                .entries_by_index
                .entry(batch_index)
                .or_insert_with(|| JoinEntry {
                    source_event_id,
                    event_type: source_event_type,
                    payload: source_payload,
                    batch_id,
                    batch_index,
                    batch_size,
                    created_at: source_created_at,
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

    pub async fn join_same_batch_complete(
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

    pub async fn join_same_batch_release(
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

    pub async fn expire_same_batch_windows(
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

            let mut source_event_ids = window.source_event_ids.iter().copied().collect::<Vec<_>>();
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
}
