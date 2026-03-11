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
use uuid::Uuid;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::event_log::EventLog;
use crate::reactor_queue::ReactorQueue;
use crate::types::*;

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
    /// Checkpoint position for ReactorQueue (last fully processed EventLog position).
    checkpoint: Arc<AtomicU64>,
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
            checkpoint: Arc::new(AtomicU64::new(0)),
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

    /// Insert a queued reactor directly (for test setup).
    pub async fn publish_reactor_for_test(&self, reactor: QueuedReactor) {
        self.reactor_queue.lock().push_back(reactor);
    }

    /// Set the checkpoint position directly (for test setup / resume simulation).
    pub fn set_checkpoint(&self, position: u64) {
        self.checkpoint.store(position, Ordering::SeqCst);
    }

    /// Access the global event log (for test assertions).
    pub fn global_log(&self) -> &Mutex<Vec<PersistedEvent>> {
        &self.global_log
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
            ephemeral: event.ephemeral,
            persistent: event.persistent,
        });

        Ok(AppendResult { position, version })
    }

    async fn load_from(
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

    async fn latest_position(&self) -> Result<u64> {
        let log = self.global_log.lock();
        Ok(log.last().map(|e| e.position).unwrap_or(0))
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
            let mut entry = self
                .reactor_descriptions
                .entry(commit.correlation_id)
                .or_default();
            entry.extend(commit.reactor_descriptions);
        }

        // Handle projection failures as DLQ entries
        for failure in commit.projection_failures {
            eprintln!(
                "Projection DLQ: {}:{} - {} ({})",
                commit.event_id, failure.reactor_id, failure.error, failure.reason
            );
        }

        // Handle park (DLQ for events)
        if let Some(park) = &commit.park {
            eprintln!(
                "Event parked: {} - {}",
                commit.event_id, park.reason
            );
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
        self.checkpoint.store(commit.checkpoint, Ordering::SeqCst);

        Ok(())
    }

    async fn checkpoint(&self) -> Result<u64> {
        Ok(self.checkpoint.load(Ordering::SeqCst))
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
                self.completed_reactors
                    .insert((completion.event_id, completion.reactor_id), completion.result);
            }
            ReactorResolution::Retry {
                event_id,
                reactor_id,
                error,
                new_attempts,
                next_execute_at,
            } => {
                tracing::warn!(
                    "Reactor retry: {}:{} - {} (attempt {})",
                    event_id,
                    reactor_id,
                    error,
                    new_attempts
                );
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
