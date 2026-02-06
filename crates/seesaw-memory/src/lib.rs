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
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use tokio::sync::broadcast;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use uuid::Uuid;

/// In-memory store for workflows
#[derive(Clone)]
pub struct MemoryStore {
    /// Event queue (FIFO per correlation_id)
    events: Arc<DashMap<Uuid, VecDeque<QueuedEvent>>>,
    /// Global event sequence for IDs
    event_seq: Arc<AtomicI64>,
    /// Workflow states
    states: Arc<DashMap<Uuid, (serde_json::Value, i32)>>,
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
    created_at: DateTime<Utc>,
}

/// Stored effect for history/tree reconstruction
#[derive(Debug, Clone)]
struct StoredEffect {
    effect_id: String,
    event_id: Uuid,
    correlation_id: Uuid,
    status: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
    attempts: i32,
    created_at: DateTime<Utc>,
}

impl MemoryStore {
    pub fn new() -> Self {
        let (insight_tx, _) = broadcast::channel(1000);
        Self {
            events: Arc::new(DashMap::new()),
            event_seq: Arc::new(AtomicI64::new(1)),
            states: Arc::new(DashMap::new()),
            effects: Arc::new(Mutex::new(VecDeque::new())),
            completed_effects: Arc::new(DashMap::new()),
            insight_tx: Arc::new(insight_tx),
            insight_seq: Arc::new(AtomicI64::new(1)),
            event_history: Arc::new(DashMap::new()),
            effect_history: Arc::new(DashMap::new()),
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
            payload: Some(event.payload.clone()),
            created_at: event.created_at,
        });

        // Add to queue
        let mut queue = self.events.entry(event.correlation_id).or_insert_with(VecDeque::new);
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

    async fn load_state<S>(&self, correlation_id: Uuid) -> Result<Option<(S, i32)>>
    where
        S: for<'de> Deserialize<'de> + Send,
    {
        if let Some(entry) = self.states.get(&correlation_id) {
            let (json, version) = entry.value();
            let state: S = serde_json::from_value(json.clone())?;
            Ok(Some((state, *version)))
        } else {
            Ok(None)
        }
    }

    async fn save_state<S>(
        &self,
        correlation_id: Uuid,
        state: &S,
        expected_version: i32,
    ) -> Result<i32>
    where
        S: Serialize + Send + Sync,
    {
        let json = serde_json::to_value(state)?;
        let new_version = expected_version + 1;

        // Simple optimistic locking check
        if let Some(mut entry) = self.states.get_mut(&correlation_id) {
            let (_, current_version) = entry.value();
            if *current_version != expected_version {
                return Err(anyhow!("Version mismatch: expected {} but was {}", expected_version, current_version));
            }
            *entry.value_mut() = (json, new_version);
        } else {
            self.states.insert(correlation_id, (json, new_version));
        }

        Ok(new_version)
    }

    async fn insert_effect_intent(
        &self,
        event_id: Uuid,
        effect_id: String,
        correlation_id: Uuid,
        event_type: String,
        event_payload: serde_json::Value,
        parent_event_id: Option<Uuid>,
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
                status: "pending".to_string(),
                result: None,
                error: None,
                attempts: 0,
                created_at: now,
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
            Ok(queue.remove(pos))
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
        self.completed_effects.insert((event_id, effect_id.clone()), result.clone());

        // Update effect history
        if let Some(mut entry) = self.effect_history.get_mut(&(event_id, effect_id.clone())) {
            entry.status = "completed".to_string();
            entry.result = Some(result.clone());

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
                created_at: Utc::now(),
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
        self.complete_effect(event_id, effect_id.clone(), result).await?;

        // Publish emitted events
        for emitted in emitted_events {
            let new_event_id = Uuid::new_v5(&NAMESPACE_SEESAW, format!("{}-{}-{}", event_id, effect_id, emitted.event_type).as_bytes());

            let queued = QueuedEvent {
                id: self.event_seq.fetch_add(1, Ordering::SeqCst),
                event_id: new_event_id,
                parent_id: Some(event_id),
                correlation_id: event_id, // Simplified: use event_id as correlation for now
                event_type: emitted.event_type,
                payload: emitted.payload,
                hops: 0,
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
            entry.status = "failed".to_string();
            entry.error = Some(error.clone());
            entry.attempts += 1;

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
                created_at: Utc::now(),
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
        _error_type: String,
        attempts: i32,
    ) -> Result<()> {
        eprintln!("Effect sent to DLQ: {}:{} - {} (attempts: {})", event_id, effect_id, error, attempts);
        Ok(())
    }

    async fn subscribe_workflow_events(&self, _correlation_id: Uuid) -> Result<Box<dyn futures::Stream<Item = WorkflowEvent> + Send + Unpin>> {
        // In-memory store doesn't support subscriptions
        Err(anyhow!("Subscriptions not supported in memory store"))
    }

    async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<WorkflowStatus> {
        // Check if any events or effects are pending
        let has_events = self.events.get(&correlation_id).map(|q| !q.is_empty()).unwrap_or(false);
        let state = self.states.get(&correlation_id).map(|entry| entry.value().0.clone());
        let pending_effects = 0i64; // Simplified

        Ok(WorkflowStatus {
            correlation_id,
            state,
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

        // Build tree (find root events and recursively build children)
        let roots = self.build_event_nodes(&events, None);

        // Get state
        let state = self.states.get(&correlation_id).map(|entry| entry.value().0.clone());

        Ok(WorkflowTree {
            correlation_id,
            roots,
            state,
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
                    payload: Some(stored.payload.clone()),
                    created_at: stored.created_at,
                })
            })
            .collect();

        // Sort by seq (oldest first for consistent cursor pagination)
        events.sort_by_key(|e| e.seq);
        events.truncate(limit);

        Ok(events)
    }
}

impl MemoryStore {
    /// Build event nodes recursively
    fn build_event_nodes(&self, events: &[StoredEvent], parent_id: Option<Uuid>) -> Vec<EventNode> {
        events
            .iter()
            .filter(|e| e.parent_id == parent_id)
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
                        }
                    })
                    .collect();

                // Recursively build children
                let children = self.build_event_nodes(events, Some(event.event_id));

                EventNode {
                    event_id: event.event_id,
                    event_type: event.event_type.clone(),
                    payload: event.payload.clone(),
                    created_at: event.created_at,
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
        let all_seqs: Vec<i64> = events.iter()
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
            };
            store.publish(event).await.unwrap();
        }

        // Request events after cursor=10 (beyond all events)
        let events = store.get_recent_events(Some(10), 10).await.unwrap();
        assert_eq!(events.len(), 0, "Should return no events when cursor is beyond all seq values");
    }
}
