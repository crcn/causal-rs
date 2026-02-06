//! In-memory Store implementation for Seesaw
//!
//! This is a simple in-memory store suitable for development, testing, and demos.
//! Not suitable for production use as all data is lost on restart.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use seesaw_core::insight::*;
use seesaw_core::store::*;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::broadcast;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// In-memory store for workflows
#[derive(Clone)]
pub struct MemoryStore {
    /// Event queue (FIFO per correlation_id)
    events: Arc<DashMap<Uuid, VecDeque<QueuedEvent>>>,
    /// Global event sequence for IDs
    event_seq: Arc<AtomicI64>,
    /// Effect executions queue
    effects: Arc<Mutex<VecDeque<QueuedEffectExecution>>>,
    /// Completed effects (for idempotency)
    completed_effects: Arc<DashMap<(Uuid, String), serde_json::Value>>,

    // Insight/observability fields
    /// Broadcast channel for live events
    insight_tx: Arc<broadcast::Sender<InsightEvent>>,
    /// Insight event sequence
    insight_seq: Arc<AtomicI64>,
    /// Event history (for tree reconstruction)
    event_history: Arc<DashMap<Uuid, StoredEvent>>,
    /// Effect history
    effect_history: Arc<DashMap<(Uuid, String), StoredEffect>>,
    /// Dead letter history
    dead_letter_history: Arc<DashMap<(Uuid, String), StoredDeadLetter>>,
    /// Durable-ish join windows for same-batch fan-in (in-memory only).
    join_windows: Arc<Mutex<HashMap<(String, Uuid, Uuid), MemoryJoinWindow>>>,
}

/// Stored event for history/tree reconstruction
#[derive(Debug, Clone)]
struct StoredEvent {
    seq: i64,
    event_id: Uuid,
    parent_id: Option<Uuid>,
    correlation_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    hops: i32,
    retry_count: i32,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    created_at: DateTime<Utc>,
}

/// Stored effect for history/tree reconstruction
#[derive(Debug, Clone)]
struct StoredEffect {
    effect_id: String,
    event_id: Uuid,
    correlation_id: Uuid,
    event_type: String,
    event_payload: serde_json::Value,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    status: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
    attempts: i32,
    created_at: DateTime<Utc>,
    execute_at: DateTime<Utc>,
    claimed_at: Option<DateTime<Utc>>,
    last_attempted_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
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
}

/// Stored dead letter entry for insight diagnostics.
#[derive(Debug, Clone)]
struct StoredDeadLetter {
    event_id: Uuid,
    effect_id: String,
    correlation_id: Uuid,
    event_type: String,
    event_payload: serde_json::Value,
    error: String,
    reason: String,
    attempts: i32,
    failed_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        let (insight_tx, _) = broadcast::channel(1000);
        Self {
            events: Arc::new(DashMap::new()),
            event_seq: Arc::new(AtomicI64::new(1)),
            effects: Arc::new(Mutex::new(VecDeque::new())),
            completed_effects: Arc::new(DashMap::new()),
            insight_tx: Arc::new(insight_tx),
            insight_seq: Arc::new(AtomicI64::new(1)),
            event_history: Arc::new(DashMap::new()),
            effect_history: Arc::new(DashMap::new()),
            dead_letter_history: Arc::new(DashMap::new()),
            join_windows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Publish insight event to broadcast channel
    fn publish_insight(&self, event: InsightEvent) {
        // Ignore send errors (no subscribers is fine)
        let _ = self.insight_tx.send(event);
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn publish(&self, event: QueuedEvent) -> Result<()> {
        if self.event_history.contains_key(&event.event_id) {
            return Ok(());
        }

        // Generate sequence number
        let seq = self.insight_seq.fetch_add(1, Ordering::SeqCst);

        // Store in event history with seq
        self.event_history.insert(
            event.event_id,
            StoredEvent {
                seq,
                event_id: event.event_id,
                parent_id: event.parent_id,
                correlation_id: event.correlation_id,
                event_type: event.event_type.clone(),
                payload: event.payload.clone(),
                hops: event.hops,
                retry_count: event.retry_count,
                batch_id: event.batch_id,
                batch_index: event.batch_index,
                batch_size: event.batch_size,
                created_at: event.created_at,
            },
        );

        // Publish insight event
        self.publish_insight(InsightEvent {
            seq,
            stream_type: StreamType::EventDispatched,
            correlation_id: event.correlation_id,
            event_id: Some(event.event_id),
            effect_event_id: None,
            effect_id: None,
            event_type: Some(event.event_type.clone()),
            status: None,
            error: None,
            payload: Some(serde_json::json!({
                "event_type": event.event_type.clone(),
                "hops": event.hops,
                "batch_id": event.batch_id,
                "batch_index": event.batch_index,
                "batch_size": event.batch_size,
                "payload": event.payload.clone(),
            })),
            created_at: event.created_at,
        });

        // Add to queue
        let mut queue = self
            .events
            .entry(event.correlation_id)
            .or_insert_with(VecDeque::new);
        queue.push_back(event);
        Ok(())
    }

    async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
        // Simple round-robin across workflows
        for mut entry in self.events.iter_mut() {
            if let Some(event) = entry.value_mut().pop_front() {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    async fn ack(&self, _id: i64) -> Result<()> {
        // No-op for in-memory (event already removed in poll_next)
        Ok(())
    }

    async fn nack(&self, _id: i64, _retry_after_secs: u64) -> Result<()> {
        // For simplicity, just drop failed events in memory store
        Ok(())
    }

    async fn insert_effect_intent(
        &self,
        event_id: Uuid,
        effect_id: String,
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
    ) -> Result<()> {
        let execution = QueuedEffectExecution {
            event_id,
            effect_id: effect_id.clone(),
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
            attempts: 0,
        };

        // Store in effect history
        let now = Utc::now();
        self.effect_history.insert(
            (event_id, effect_id.clone()),
            StoredEffect {
                effect_id: effect_id.clone(),
                event_id,
                correlation_id,
                event_type: execution.event_type.clone(),
                event_payload: execution.event_payload.clone(),
                batch_id: execution.batch_id,
                batch_index: execution.batch_index,
                batch_size: execution.batch_size,
                status: "pending".to_string(),
                result: None,
                error: None,
                attempts: 0,
                created_at: now,
                execute_at,
                claimed_at: None,
                last_attempted_at: None,
                completed_at: None,
            },
        );

        // Publish insight event
        let seq = self.insight_seq.fetch_add(1, Ordering::SeqCst);
        self.publish_insight(InsightEvent {
            seq,
            stream_type: StreamType::EffectStarted,
            correlation_id,
            event_id: None,
            effect_event_id: Some(event_id),
            effect_id: Some(effect_id),
            event_type: None,
            status: Some("pending".to_string()),
            error: None,
            payload: None,
            created_at: now,
        });

        let mut queue = self.effects.lock();
        queue.push_back(execution);
        Ok(())
    }

    async fn poll_next_effect(&self) -> Result<Option<QueuedEffectExecution>> {
        let mut queue = self.effects.lock();

        // Find first effect that's ready to execute
        let now = Utc::now();
        if let Some(pos) = queue.iter().position(|e| e.execute_at <= now) {
            if let Some(next) = queue.remove(pos) {
                if let Some(mut effect) = self
                    .effect_history
                    .get_mut(&(next.event_id, next.effect_id.clone()))
                {
                    effect.status = "executing".to_string();
                    effect.claimed_at = Some(now);
                    effect.last_attempted_at = Some(now);
                    effect.attempts = next.attempts + 1;
                }
                Ok(Some(next))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    async fn complete_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        result: serde_json::Value,
    ) -> Result<()> {
        self.completed_effects
            .insert((event_id, effect_id.clone()), result.clone());

        // Update effect history
        if let Some(mut entry) = self.effect_history.get_mut(&(event_id, effect_id.clone())) {
            entry.status = "completed".to_string();
            entry.result = Some(result.clone());
            let completed_at = Utc::now();
            entry.completed_at = Some(completed_at);
            entry.last_attempted_at = Some(completed_at);

            // Publish insight event
            let seq = self.insight_seq.fetch_add(1, Ordering::SeqCst);
            self.publish_insight(InsightEvent {
                seq,
                stream_type: StreamType::EffectCompleted,
                correlation_id: entry.correlation_id,
                event_id: None,
                effect_event_id: Some(event_id),
                effect_id: Some(effect_id),
                event_type: None,
                status: Some("completed".to_string()),
                error: None,
                payload: Some(result),
                created_at: completed_at,
            });
        }

        Ok(())
    }

    async fn complete_effect_with_events(
        &self,
        event_id: Uuid,
        effect_id: String,
        result: serde_json::Value,
        emitted_events: Vec<EmittedEvent>,
    ) -> Result<()> {
        // Mark effect complete
        self.complete_effect(event_id, effect_id.clone(), result)
            .await?;

        let correlation_id = self
            .effect_history
            .get(&(event_id, effect_id.clone()))
            .map(|entry| entry.correlation_id)
            .or_else(|| {
                self.event_history
                    .get(&event_id)
                    .map(|entry| entry.correlation_id)
            })
            .unwrap_or(event_id);
        let parent_hops = self
            .event_history
            .get(&event_id)
            .map(|entry| entry.hops)
            .unwrap_or(0);

        // Publish emitted events
        for (emitted_index, emitted) in emitted_events.into_iter().enumerate() {
            let new_event_id = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!(
                    "{}-{}-{}-{}",
                    event_id, effect_id, emitted.event_type, emitted_index
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

    async fn fail_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        error: String,
        _retry_after_secs: i32,
    ) -> Result<()> {
        // Update effect history
        if let Some(mut entry) = self.effect_history.get_mut(&(event_id, effect_id.clone())) {
            let failed_at = Utc::now();
            entry.status = "failed".to_string();
            entry.error = Some(error.clone());
            entry.attempts += 1;
            entry.last_attempted_at = Some(failed_at);

            // Publish insight event
            let seq = self.insight_seq.fetch_add(1, Ordering::SeqCst);
            self.publish_insight(InsightEvent {
                seq,
                stream_type: StreamType::EffectFailed,
                correlation_id: entry.correlation_id,
                event_id: None,
                effect_event_id: Some(event_id),
                effect_id: Some(effect_id),
                event_type: None,
                status: Some("failed".to_string()),
                error: Some(error.clone()),
                payload: None,
                created_at: failed_at,
            });
        }

        eprintln!("Effect failed: {}", error);
        Ok(())
    }

    async fn dlq_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        error: String,
        error_type: String,
        attempts: i32,
    ) -> Result<()> {
        self.dlq_effect_with_events(event_id, effect_id, error, error_type, attempts, Vec::new())
            .await
    }

    async fn dlq_effect_with_events(
        &self,
        event_id: Uuid,
        effect_id: String,
        error: String,
        error_type: String,
        attempts: i32,
        emitted_events: Vec<EmittedEvent>,
    ) -> Result<()> {
        let effect_snapshot =
            self.effect_history
                .get(&(event_id, effect_id.clone()))
                .map(|entry| {
                    (
                        entry.correlation_id,
                        entry.event_type.clone(),
                        entry.event_payload.clone(),
                        entry.batch_id,
                        entry.batch_index,
                        entry.batch_size,
                    )
                });
        let event_snapshot = self.event_history.get(&event_id).map(|event| {
            (
                event.correlation_id,
                event.event_type.clone(),
                event.payload.clone(),
                event.batch_id,
                event.batch_index,
                event.batch_size,
                event.hops,
            )
        });

        let (correlation_id, event_type, event_payload, batch_id, batch_index, batch_size) =
            if let Some(snapshot) = effect_snapshot.clone() {
                (
                    snapshot.0, snapshot.1, snapshot.2, snapshot.3, snapshot.4, snapshot.5,
                )
            } else if let Some(snapshot) = event_snapshot.clone() {
                (
                    snapshot.0, snapshot.1, snapshot.2, snapshot.3, snapshot.4, snapshot.5,
                )
            } else {
                (
                    event_id,
                    "unknown".to_string(),
                    serde_json::Value::Null,
                    None,
                    None,
                    None,
                )
            };

        if let Some(mut effect) = self.effect_history.get_mut(&(event_id, effect_id.clone())) {
            effect.status = "failed".to_string();
            effect.error = Some(error.clone());
            effect.attempts = attempts;
            effect.last_attempted_at = Some(Utc::now());
        }

        self.dead_letter_history.insert(
            (event_id, effect_id.clone()),
            StoredDeadLetter {
                event_id,
                effect_id: effect_id.clone(),
                correlation_id,
                event_type: event_type.clone(),
                event_payload: event_payload.clone(),
                error: error.clone(),
                reason: error_type,
                attempts,
                failed_at: Utc::now(),
                resolved_at: None,
            },
        );

        let parent_hops = event_snapshot.map(|snapshot| snapshot.6).unwrap_or(0);
        if emitted_events.is_empty() {
            if let (Some(batch_id), Some(batch_index), Some(batch_size)) =
                (batch_id, batch_index, batch_size)
            {
                let synthetic_event_id = Uuid::new_v5(
                    &NAMESPACE_SEESAW,
                    format!("{}-{}-dlq-terminal", event_id, effect_id).as_bytes(),
                );
                self.publish(QueuedEvent {
                    id: self.event_seq.fetch_add(1, Ordering::SeqCst),
                    event_id: synthetic_event_id,
                    parent_id: Some(event_id),
                    correlation_id,
                    event_type: event_type.clone(),
                    payload: event_payload.clone(),
                    hops: parent_hops + 1,
                    retry_count: 0,
                    batch_id: Some(batch_id),
                    batch_index: Some(batch_index),
                    batch_size: Some(batch_size),
                    created_at: Utc::now(),
                })
                .await?;
            }
        } else {
            for (emitted_index, emitted) in emitted_events.into_iter().enumerate() {
                let synthetic_event_id = Uuid::new_v5(
                    &NAMESPACE_SEESAW,
                    format!(
                        "{}-{}-dlq-terminal-{}-{}",
                        event_id, effect_id, emitted.event_type, emitted_index
                    )
                    .as_bytes(),
                );
                self.publish(QueuedEvent {
                    id: self.event_seq.fetch_add(1, Ordering::SeqCst),
                    event_id: synthetic_event_id,
                    parent_id: Some(event_id),
                    correlation_id,
                    event_type: emitted.event_type,
                    payload: emitted.payload,
                    hops: parent_hops + 1,
                    retry_count: 0,
                    batch_id: emitted.batch_id.or(batch_id),
                    batch_index: emitted.batch_index.or(batch_index),
                    batch_size: emitted.batch_size.or(batch_size),
                    created_at: Utc::now(),
                })
                .await?;
            }
        }

        eprintln!(
            "Effect sent to DLQ: {}:{} - {} (attempts: {})",
            event_id, effect_id, error, attempts
        );
        Ok(())
    }

    async fn join_same_batch_append_and_maybe_claim(
        &self,
        join_effect_id: String,
        correlation_id: Uuid,
        source_event_id: Uuid,
        source_event_type: String,
        source_payload: serde_json::Value,
        source_created_at: DateTime<Utc>,
        batch_id: Uuid,
        batch_index: i32,
        batch_size: i32,
    ) -> Result<Option<Vec<JoinEntry>>> {
        let key = (join_effect_id.clone(), correlation_id, batch_id);
        let mut windows = self.join_windows.lock();
        let window = windows.entry(key).or_insert_with(|| MemoryJoinWindow {
            target_count: batch_size,
            status: MemoryJoinStatus::Open,
            source_event_ids: HashSet::new(),
            entries_by_index: HashMap::new(),
        });

        if window.status == MemoryJoinStatus::Completed {
            return Ok(None);
        }

        if window.target_count != batch_size {
            window.target_count = batch_size;
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

    async fn join_same_batch_complete(
        &self,
        join_effect_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
    ) -> Result<()> {
        let key = (join_effect_id, correlation_id, batch_id);
        if let Some(window) = self.join_windows.lock().get_mut(&key) {
            window.status = MemoryJoinStatus::Completed;
        }
        self.join_windows.lock().remove(&key);
        Ok(())
    }

    async fn join_same_batch_release(
        &self,
        join_effect_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
        _error: String,
    ) -> Result<()> {
        let key = (join_effect_id, correlation_id, batch_id);
        if let Some(window) = self.join_windows.lock().get_mut(&key) {
            if window.status == MemoryJoinStatus::Processing {
                window.status = MemoryJoinStatus::Open;
            }
        }
        Ok(())
    }

    async fn subscribe_workflow_events(
        &self,
        _correlation_id: Uuid,
    ) -> Result<Box<dyn futures::Stream<Item = WorkflowEvent> + Send + Unpin>> {
        // In-memory store doesn't support subscriptions
        Err(anyhow!("Subscriptions not supported in memory store"))
    }

    async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<WorkflowStatus> {
        // Check if any events or effects are pending
        let has_events = self
            .events
            .get(&correlation_id)
            .map(|q| !q.is_empty())
            .unwrap_or(false);
        let pending_effects = 0i64; // Simplified

        Ok(WorkflowStatus {
            correlation_id,
            pending_effects,
            is_settled: !has_events && pending_effects == 0,
            last_event: None, // Could track this if needed
        })
    }
}

#[async_trait]
impl InsightStore for MemoryStore {
    async fn subscribe_events(
        &self,
    ) -> Result<Box<dyn futures::Stream<Item = InsightEvent> + Send + Unpin>> {
        let mut rx = self.insight_tx.subscribe();
        let stream = async_stream::stream! {
            while let Ok(event) = rx.recv().await {
                yield event;
            }
        };

        Ok(Box::new(Box::pin(stream)))
    }

    async fn get_workflow_tree(&self, correlation_id: Uuid) -> Result<WorkflowTree> {
        // Find all events for this correlation
        let mut events: Vec<_> = self
            .event_history
            .iter()
            .filter(|e| e.value().correlation_id == correlation_id)
            .map(|e| e.value().clone())
            .collect();

        events.sort_by_key(|e| e.created_at);

        // Build tree. Treat events with missing parents as roots so partially
        // captured workflows still render.
        let event_ids: HashSet<Uuid> = events.iter().map(|e| e.event_id).collect();
        let roots = self.build_event_nodes(&events, None, &event_ids, true);

        Ok(WorkflowTree {
            correlation_id,
            roots,
            event_count: events.len(),
            effect_count: self
                .effect_history
                .iter()
                .filter(|e| e.value().correlation_id == correlation_id)
                .count(),
        })
    }

    async fn get_stats(&self) -> Result<InsightStats> {
        let total_events = self.event_history.len() as i64;

        let mut active_effects = 0i64;
        let mut completed_effects = 0i64;
        let mut failed_effects = 0i64;

        for entry in self.effect_history.iter() {
            match entry.value().status.as_str() {
                "pending" | "executing" => active_effects += 1,
                "completed" => completed_effects += 1,
                "failed" => failed_effects += 1,
                _ => {}
            }
        }

        Ok(InsightStats {
            total_events,
            active_effects,
            completed_effects,
            failed_effects,
        })
    }

    async fn get_recent_events(
        &self,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<Vec<InsightEvent>> {
        // Get events from history with proper seq
        let mut events: Vec<_> = self
            .event_history
            .iter()
            .filter_map(|e| {
                let stored = e.value();
                // Filter by cursor if provided
                if let Some(cursor_seq) = cursor {
                    if stored.seq <= cursor_seq {
                        return None;
                    }
                }
                Some(InsightEvent {
                    seq: stored.seq,
                    stream_type: StreamType::EventDispatched,
                    correlation_id: stored.correlation_id,
                    event_id: Some(stored.event_id),
                    effect_event_id: None,
                    effect_id: None,
                    event_type: Some(stored.event_type.clone()),
                    status: None,
                    error: None,
                    payload: Some(serde_json::json!({
                        "event_type": stored.event_type.clone(),
                        "hops": stored.hops,
                        "batch_id": stored.batch_id,
                        "batch_index": stored.batch_index,
                        "batch_size": stored.batch_size,
                        "payload": stored.payload.clone(),
                    })),
                    created_at: stored.created_at,
                })
            })
            .collect();

        // Sort by seq (oldest first for consistent cursor pagination)
        events.sort_by_key(|e| e.seq);
        events.truncate(limit);

        Ok(events)
    }

    async fn get_effect_logs(
        &self,
        correlation_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<EffectExecutionLog>> {
        let mut logs: Vec<_> = self
            .effect_history
            .iter()
            .filter_map(|entry| {
                let effect = entry.value();
                if let Some(filter) = correlation_id {
                    if effect.correlation_id != filter {
                        return None;
                    }
                }

                let started_at = effect.claimed_at.or(effect.last_attempted_at);
                let duration_ms = match (started_at, effect.completed_at) {
                    (Some(start), Some(end)) => Some((end - start).num_milliseconds().max(0)),
                    _ => None,
                };

                Some(EffectExecutionLog {
                    correlation_id: effect.correlation_id,
                    event_id: effect.event_id,
                    effect_id: effect.effect_id.clone(),
                    status: effect.status.clone(),
                    attempts: effect.attempts,
                    event_type: Some(effect.event_type.clone()),
                    result: effect.result.clone(),
                    error: effect.error.clone(),
                    created_at: effect.created_at,
                    execute_at: Some(effect.execute_at),
                    claimed_at: effect.claimed_at,
                    last_attempted_at: effect.last_attempted_at,
                    completed_at: effect.completed_at,
                    duration_ms,
                })
            })
            .collect();

        logs.sort_by(|a, b| {
            let a_time = a
                .last_attempted_at
                .or(a.completed_at)
                .or(a.claimed_at)
                .unwrap_or(a.created_at);
            let b_time = b
                .last_attempted_at
                .or(b.completed_at)
                .or(b.claimed_at)
                .unwrap_or(b.created_at);
            b_time.cmp(&a_time)
        });
        logs.truncate(limit);
        Ok(logs)
    }

    async fn get_dead_letters(
        &self,
        unresolved_only: bool,
        limit: usize,
    ) -> Result<Vec<DeadLetterEntry>> {
        let mut rows: Vec<_> = self
            .dead_letter_history
            .iter()
            .filter_map(|entry| {
                let dead = entry.value();
                if unresolved_only && dead.resolved_at.is_some() {
                    return None;
                }

                Some(DeadLetterEntry {
                    correlation_id: dead.correlation_id,
                    event_id: dead.event_id,
                    effect_id: dead.effect_id.clone(),
                    event_type: dead.event_type.clone(),
                    event_payload: dead.event_payload.clone(),
                    error: dead.error.clone(),
                    reason: dead.reason.clone(),
                    attempts: dead.attempts,
                    failed_at: dead.failed_at,
                    resolved_at: dead.resolved_at,
                })
            })
            .collect();

        rows.sort_by(|a, b| b.failed_at.cmp(&a.failed_at));
        rows.truncate(limit);
        Ok(rows)
    }

    async fn get_failed_workflows(&self, limit: usize) -> Result<Vec<FailedWorkflow>> {
        let mut workflows: HashMap<Uuid, FailedWorkflow> = HashMap::new();

        for entry in self.effect_history.iter() {
            let effect = entry.value();
            let workflow = workflows
                .entry(effect.correlation_id)
                .or_insert(FailedWorkflow {
                    correlation_id: effect.correlation_id,
                    failed_effects: 0,
                    active_effects: 0,
                    dead_letters: 0,
                    last_failed_at: None,
                    last_error: None,
                });

            match effect.status.as_str() {
                "failed" => {
                    workflow.failed_effects += 1;
                    let at = effect.last_attempted_at.unwrap_or(effect.created_at);
                    if workflow
                        .last_failed_at
                        .map(|current| at > current)
                        .unwrap_or(true)
                    {
                        workflow.last_failed_at = Some(at);
                        workflow.last_error = effect.error.clone();
                    }
                }
                "pending" | "executing" => {
                    workflow.active_effects += 1;
                }
                _ => {}
            }
        }

        for entry in self.dead_letter_history.iter() {
            let dead = entry.value();
            if dead.resolved_at.is_some() {
                continue;
            }

            let workflow = workflows
                .entry(dead.correlation_id)
                .or_insert(FailedWorkflow {
                    correlation_id: dead.correlation_id,
                    failed_effects: 0,
                    active_effects: 0,
                    dead_letters: 0,
                    last_failed_at: None,
                    last_error: None,
                });

            workflow.dead_letters += 1;
            if workflow
                .last_failed_at
                .map(|current| dead.failed_at > current)
                .unwrap_or(true)
            {
                workflow.last_failed_at = Some(dead.failed_at);
                workflow.last_error = Some(dead.error.clone());
            }
        }

        let mut rows: Vec<_> = workflows
            .into_values()
            .filter(|workflow| workflow.failed_effects > 0 || workflow.dead_letters > 0)
            .collect();

        rows.sort_by(|a, b| b.last_failed_at.cmp(&a.last_failed_at));
        rows.truncate(limit);
        Ok(rows)
    }
}

impl MemoryStore {
    /// Build event nodes recursively
    fn build_event_nodes(
        &self,
        events: &[StoredEvent],
        parent_id: Option<Uuid>,
        event_ids: &HashSet<Uuid>,
        is_root_pass: bool,
    ) -> Vec<EventNode> {
        events
            .iter()
            .filter(|event| {
                if is_root_pass {
                    event.parent_id.is_none()
                        || event
                            .parent_id
                            .map(|parent| !event_ids.contains(&parent))
                            .unwrap_or(false)
                } else {
                    event.parent_id == parent_id
                }
            })
            .map(|event| {
                // Get effects for this event
                let effects = self
                    .effect_history
                    .iter()
                    .filter(|e| e.value().event_id == event.event_id)
                    .map(|e| {
                        let effect = e.value();
                        EffectNode {
                            effect_id: effect.effect_id.clone(),
                            event_id: effect.event_id,
                            status: effect.status.clone(),
                            result: effect.result.clone(),
                            error: effect.error.clone(),
                            attempts: effect.attempts,
                            created_at: effect.created_at,
                            batch_id: effect.batch_id,
                            batch_index: effect.batch_index,
                            batch_size: effect.batch_size,
                        }
                    })
                    .collect();

                // Recursively build children
                let children =
                    self.build_event_nodes(events, Some(event.event_id), event_ids, false);

                EventNode {
                    event_id: event.event_id,
                    event_type: event.event_type.clone(),
                    payload: event.payload.clone(),
                    created_at: event.created_at,
                    batch_id: event.batch_id,
                    batch_index: event.batch_index,
                    batch_size: event.batch_size,
                    children,
                    effects,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seesaw_core::store::Store;

    #[tokio::test]
    async fn test_insight_events_have_unique_seq() {
        let store = MemoryStore::new();

        // Publish 3 events
        for i in 1..=3 {
            let event = QueuedEvent {
                id: i as i64,
                event_id: Uuid::new_v4(),
                parent_id: None,
                correlation_id: Uuid::new_v4(),
                event_type: format!("Event{}", i),
                payload: serde_json::json!({"n": i}),
                created_at: Utc::now(),
                hops: 0,
                retry_count: 0,
                batch_id: None,
                batch_index: None,
                batch_size: None,
            };
            store.publish(event).await.unwrap();
        }

        // Get recent events - should have seq 1, 2, 3
        let events = store.get_recent_events(None, 10).await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[1].seq, 2);
        assert_eq!(events[2].seq, 3);
    }

    #[tokio::test]
    async fn test_cursor_based_filtering() {
        let store = MemoryStore::new();
        let correlation_id = Uuid::new_v4();

        // Publish 5 events
        for i in 1..=5 {
            let event = QueuedEvent {
                id: i as i64,
                event_id: Uuid::new_v4(),
                parent_id: None,
                correlation_id,
                event_type: format!("Event{}", i),
                payload: serde_json::json!({"n": i}),
                created_at: Utc::now(),
                hops: 0,
                retry_count: 0,
                batch_id: None,
                batch_index: None,
                batch_size: None,
            };
            store.publish(event).await.unwrap();
        }

        // Get first 2 events (no cursor)
        let events = store.get_recent_events(None, 2).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[1].seq, 2);

        // Get next events after cursor=2
        let next_events = store.get_recent_events(Some(2), 2).await.unwrap();
        assert_eq!(next_events.len(), 2);
        assert_eq!(next_events[0].seq, 3);
        assert_eq!(next_events[1].seq, 4);

        // Verify no duplicates between historical and cursor fetch
        let all_seqs: Vec<i64> = events
            .iter()
            .chain(next_events.iter())
            .map(|e| e.seq)
            .collect();
        assert_eq!(all_seqs, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_no_events_before_cursor() {
        let store = MemoryStore::new();

        // Publish 3 events
        for i in 1..=3 {
            let event = QueuedEvent {
                id: i as i64,
                event_id: Uuid::new_v4(),
                parent_id: None,
                correlation_id: Uuid::new_v4(),
                event_type: format!("Event{}", i),
                payload: serde_json::json!({"n": i}),
                created_at: Utc::now(),
                hops: 0,
                retry_count: 0,
                batch_id: None,
                batch_index: None,
                batch_size: None,
            };
            store.publish(event).await.unwrap();
        }

        // Request events after cursor=10 (beyond all events)
        let events = store.get_recent_events(Some(10), 10).await.unwrap();
        assert_eq!(
            events.len(),
            0,
            "Should return no events when cursor is beyond all seq values"
        );
    }

    #[tokio::test]
    async fn test_workflow_tree_treats_orphan_parent_as_root() {
        let store = MemoryStore::new();
        let correlation_id = Uuid::new_v4();
        let event_id = Uuid::new_v4();
        let missing_parent = Uuid::new_v4();

        store
            .publish(QueuedEvent {
                id: 1,
                event_id,
                parent_id: Some(missing_parent),
                correlation_id,
                event_type: "OrphanEvent".to_string(),
                payload: serde_json::json!({"ok": true}),
                created_at: Utc::now(),
                hops: 1,
                retry_count: 0,
                batch_id: None,
                batch_index: None,
                batch_size: None,
            })
            .await
            .unwrap();

        let tree = store.get_workflow_tree(correlation_id).await.unwrap();
        assert_eq!(tree.event_count, 1);
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].event_id, event_id);
    }

    #[tokio::test]
    async fn test_dlq_with_batch_metadata_publishes_synthetic_terminal_event() {
        let store = MemoryStore::new();
        let event_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();

        store
            .insert_effect_intent(
                event_id,
                "join_effect".to_string(),
                correlation_id,
                "BatchItemResult".to_string(),
                serde_json::json!({ "index": 2, "ok": false }),
                None,
                Some(batch_id),
                Some(2),
                Some(5),
                Utc::now(),
                30,
                1,
                10,
            )
            .await
            .expect("insert should succeed");

        store
            .dlq_effect(
                event_id,
                "join_effect".to_string(),
                "forced failure".to_string(),
                "failed".to_string(),
                1,
            )
            .await
            .expect("dlq should succeed");

        let synthetic = store.poll_next().await.expect("poll should succeed");
        assert!(
            synthetic.is_some(),
            "synthetic terminal event should be published"
        );
        let synthetic = synthetic.unwrap();
        assert_eq!(synthetic.correlation_id, correlation_id);
        assert_eq!(synthetic.event_type, "BatchItemResult");
        assert_eq!(synthetic.batch_id, Some(batch_id));
        assert_eq!(synthetic.batch_index, Some(2));
        assert_eq!(synthetic.batch_size, Some(5));
    }

    #[tokio::test]
    async fn test_dlq_without_batch_metadata_does_not_publish_synthetic_terminal_event() {
        let store = MemoryStore::new();
        let event_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();

        store
            .insert_effect_intent(
                event_id,
                "normal_effect".to_string(),
                correlation_id,
                "NormalEvent".to_string(),
                serde_json::json!({ "ok": true }),
                None,
                None,
                None,
                None,
                Utc::now(),
                30,
                1,
                10,
            )
            .await
            .expect("insert should succeed");

        store
            .dlq_effect(
                event_id,
                "normal_effect".to_string(),
                "forced failure".to_string(),
                "failed".to_string(),
                1,
            )
            .await
            .expect("dlq should succeed");

        let next = store.poll_next().await.expect("poll should succeed");
        assert!(next.is_none(), "no synthetic terminal event expected");
    }
}
