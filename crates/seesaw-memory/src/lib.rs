//! In-memory InsightStore implementation for Seesaw
//!
//! Provides an in-memory InsightStore suitable for development, testing, and demos,
//! and an in-memory ES EventStore via the `event_store` module.

pub mod event_store;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use seesaw_core::insight::*;
use seesaw_core::QueuedEvent;
use std::collections::{HashMap, HashSet};
use tokio::sync::broadcast;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// In-memory InsightStore for workflows.
///
/// Tracks event and effect history for observability. Use `record_event()`
/// and related methods to populate data, then query via InsightStore trait.
#[derive(Clone)]
pub struct MemoryStore {
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
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    created_at: DateTime<Utc>,
}

/// Stored effect for history/tree reconstruction
#[derive(Debug, Clone)]
struct StoredEffect {
    handler_id: String,
    event_id: Uuid,
    correlation_id: Uuid,
    event_type: String,
    #[allow(dead_code)]
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

/// Stored dead letter entry for insight diagnostics.
#[derive(Debug, Clone)]
struct StoredDeadLetter {
    event_id: Uuid,
    handler_id: String,
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
            insight_tx: Arc::new(insight_tx),
            insight_seq: Arc::new(AtomicI64::new(1)),
            event_history: Arc::new(DashMap::new()),
            effect_history: Arc::new(DashMap::new()),
            dead_letter_history: Arc::new(DashMap::new()),
        }
    }

    /// Record an event for insight tracking.
    ///
    /// This populates the event history used by InsightStore queries
    /// and broadcasts an insight event to live subscribers.
    pub fn record_event(&self, event: &QueuedEvent) {
        if self.event_history.contains_key(&event.event_id) {
            return;
        }

        let seq = self.insight_seq.fetch_add(1, Ordering::SeqCst);

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
                batch_id: event.batch_id,
                batch_index: event.batch_index,
                batch_size: event.batch_size,
                created_at: event.created_at,
            },
        );

        let _ = self.insight_tx.send(InsightEvent {
            seq,
            stream_type: StreamType::EventDispatched,
            correlation_id: event.correlation_id,
            event_id: Some(event.event_id),
            effect_event_id: None,
            handler_id: None,
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
    }

    /// Publish insight event to broadcast channel
    fn publish_insight(&self, event: InsightEvent) {
        let _ = self.insight_tx.send(event);
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
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

    async fn get_workflow_tree(&self, correlation_id: Uuid) -> Result<seesaw_core::WorkflowTree> {
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

        Ok(seesaw_core::WorkflowTree {
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

    async fn get_stats(&self) -> Result<seesaw_core::InsightStats> {
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

        Ok(seesaw_core::InsightStats {
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
        let mut events: Vec<_> = self
            .event_history
            .iter()
            .filter_map(|e| {
                let stored = e.value();
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
                    handler_id: None,
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
                    handler_id: effect.handler_id.clone(),
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
                    handler_id: dead.handler_id.clone(),
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
                        HandlerNode {
                            handler_id: effect.handler_id.clone(),
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

    #[tokio::test]
    async fn test_insight_events_have_unique_seq() {
        let store = MemoryStore::new();

        // Record 3 events
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
            store.record_event(&event);
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

        // Record 5 events
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
            store.record_event(&event);
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

        // Record 3 events
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
            store.record_event(&event);
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

        store.record_event(&QueuedEvent {
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
        });

        let tree = store.get_workflow_tree(correlation_id).await.unwrap();
        assert_eq!(tree.event_count, 1);
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].event_id, event_id);
    }
}
