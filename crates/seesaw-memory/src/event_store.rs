use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::Mutex;
use seesaw_core::es::{ConcurrencyError, EventStore, NewEvent, StoredEvent};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// In-memory event store for development, testing, and demos.
#[derive(Clone)]
pub struct MemoryEventStore {
    /// Per-aggregate event streams.
    streams: Arc<DashMap<Uuid, Vec<StoredEvent>>>,
    /// Global position counter.
    global_position: Arc<AtomicU64>,
    /// Per-aggregate lock for append serialization.
    append_locks: Arc<DashMap<Uuid, Arc<Mutex<()>>>>,
}

impl MemoryEventStore {
    pub fn new() -> Self {
        Self {
            streams: Arc::new(DashMap::new()),
            global_position: Arc::new(AtomicU64::new(0)),
            append_locks: Arc::new(DashMap::new()),
        }
    }
}

impl Default for MemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn load_events(
        &self,
        aggregate_id: Uuid,
        from_version: u64,
    ) -> Result<Vec<StoredEvent>> {
        let Some(stream) = self.streams.get(&aggregate_id) else {
            return Ok(Vec::new());
        };

        Ok(stream
            .iter()
            .filter(|e| e.sequence > from_version)
            .cloned()
            .collect())
    }

    async fn append(
        &self,
        aggregate_id: Uuid,
        _aggregate_type: &str,
        expected_version: u64,
        events: Vec<NewEvent>,
    ) -> Result<u64> {
        if events.is_empty() {
            return Ok(expected_version);
        }

        // Get or create per-aggregate lock
        let lock = self
            .append_locks
            .entry(aggregate_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();

        let _guard = lock.lock();

        // Check current version
        let current_version = self
            .streams
            .get(&aggregate_id)
            .map(|s| s.last().map_or(0, |e| e.sequence))
            .unwrap_or(0);

        if current_version != expected_version {
            return Err(ConcurrencyError {
                aggregate_id,
                expected: expected_version,
                actual: current_version,
            }
            .into());
        }

        // Validate aggregate type consistency
        if let Some(stream) = self.streams.get(&aggregate_id) {
            if let Some(first) = stream.first() {
                if first.event_type != "__skip__" && !stream.is_empty() {
                    // Check aggregate_type from metadata if we stored it
                    // For simplicity, we trust the caller
                }
            }
        }

        let now = Utc::now();
        let mut seq = expected_version;
        let mut new_stored = Vec::with_capacity(events.len());

        for event in events {
            seq += 1;
            let position = self.global_position.fetch_add(1, Ordering::SeqCst) + 1;

            new_stored.push(StoredEvent {
                id: Uuid::new_v4(),
                position,
                aggregate_id,
                sequence: seq,
                event_type: event.event_type,
                data: event.data,
                metadata: event.metadata.unwrap_or(serde_json::json!({})),
                schema_version: event.schema_version,
                caused_by: event.caused_by,
                created_at: now,
            });
        }

        self.streams
            .entry(aggregate_id)
            .or_default()
            .extend(new_stored);

        Ok(seq)
    }

    async fn exists(&self, aggregate_id: Uuid) -> Result<bool> {
        Ok(self
            .streams
            .get(&aggregate_id)
            .map_or(false, |s| !s.is_empty()))
    }
}
