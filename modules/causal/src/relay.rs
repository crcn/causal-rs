//! Relay loop that drains the reactor outbox into the event log.
//!
//! Per C12, the runtime guarantees:
//!   - `commit_reactor_batch` atomically writes outbox rows + advances
//!     the reactor cursor.
//!   - The relay drains pending rows by appending each to the
//!     `EventLogBackend` and then deleting the outbox row.
//!   - Crash between successful append and outbox_delete is harmless
//!     because the next drain re-appends — and the log's idempotent-
//!     append-on-event_id contract (C1) absorbs the duplicate.
//!
//! Phase 4c lands the drain primitive (`RelayLoop::drain_once`) as a
//! pure async function the engine builder can call. Phase 4d will
//! spawn this in a long-running task with backoff and shutdown.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::aggregator::AggregatorRegistry;
use crate::checkpoint_store::{OutboxRow, ReactorOutbox};
use crate::event_log::EventLogBackend;
use crate::types::NewEvent;

pub struct RelayLoop {
    log:                 Arc<dyn EventLogBackend>,
    outbox:              Arc<dyn ReactorOutbox>,
    /// Engine-level aggregator registry. Folded after each successful
    /// log.append so reactor-emitted events update the same out-of-band
    /// state that `engine.snapshot::<A>(stream_id)` reads from.
    /// `None` if the engine has no aggregators registered.
    engine_aggregators:  Option<Arc<AggregatorRegistry>>,
}

impl RelayLoop {
    pub fn new(
        log: Arc<dyn EventLogBackend>,
        outbox: Arc<dyn ReactorOutbox>,
    ) -> Self {
        Self { log, outbox, engine_aggregators: None }
    }

    /// Attach the engine's aggregator registry so reactor outputs fold
    /// into it after `log.append`. Without this, `engine.snapshot()`
    /// only sees caller-emitted events; reactor side effects on
    /// `PipelineState`-style cross-fact aggregates stay invisible.
    pub fn with_engine_aggregators(
        mut self,
        aggregators: Option<Arc<AggregatorRegistry>>,
    ) -> Self {
        self.engine_aggregators = aggregators;
        self
    }

    /// Drain up to `batch` outbox rows. Returns the number of rows
    /// successfully delivered. Per-row failures stop the drain and
    /// propagate the error; the row remains in the outbox for the
    /// next call (or operator inspection).
    pub async fn drain_once(&self, batch: usize) -> Result<usize> {
        let pending = self.outbox.outbox_pending(batch).await
            .context("outbox_pending")?;

        let mut delivered = 0;
        for row in pending {
            let new_event = self.row_to_new_event(&row);
            let event_type = new_event.event_type.clone();
            let payload = new_event.payload.clone();
            self.log.append(new_event).await
                .with_context(|| format!("append outbox row id={}", row.id))?;
            if let Some(reg) = &self.engine_aggregators {
                reg.apply_event(&event_type, &payload);
            }
            self.outbox.outbox_delete(row.id).await
                .with_context(|| format!("outbox_delete id={}", row.id))?;
            delivered += 1;
        }
        Ok(delivered)
    }

    fn row_to_new_event(&self, row: &OutboxRow) -> NewEvent {
        NewEvent {
            event_id:       row.event_id,
            parent_id:      Some(row.source_event_id),
            correlation_id: row.correlation_id,
            event_type:     row.event_type.clone(),
            payload:        row.fact_payload.clone(),
            created_at:     row.created_at,
            aggregate_type: None,
            aggregate_id:   None,
            metadata:       reactor_origin_metadata(row, Utc::now()),
            ephemeral:      None,
            persistent:     true,
        }
    }
}

fn reactor_origin_metadata(
    row: &OutboxRow,
    relay_at: DateTime<Utc>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("_reactor_id".into(), serde_json::json!(row.reactor_id));
    m.insert("_reactor_output_index".into(), serde_json::json!(row.output_index));
    m.insert("_relay_drained_at".into(), serde_json::json!(relay_at));
    m
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_store::MemoryStore;
    use crate::types::LogCursor;
    use uuid::Uuid;

    fn make_row(event_id: Uuid, idx: u32) -> crate::checkpoint_store::InsertableOutboxRow {
        crate::checkpoint_store::InsertableOutboxRow {
            reactor_id:      "r.test".into(),
            source_event_id: Uuid::new_v4(),
            output_index:    idx,
            event_id,
            event_type:      "test.fact".into(),
            fact_payload:    serde_json::json!({"idx": idx}),
            correlation_id:  Uuid::new_v4(),
        }
    }

    #[tokio::test]
    async fn drain_once_empties_outbox_into_log() {
        let store = Arc::new(MemoryStore::new());
        store.commit_reactor_batch(
            vec![
                make_row(Uuid::new_v4(), 0),
                make_row(Uuid::new_v4(), 1),
                make_row(Uuid::new_v4(), 2),
            ],
            None,
        ).await.unwrap();

        let relay = RelayLoop::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );

        let n = relay.drain_once(10).await.unwrap();
        assert_eq!(n, 3);

        // Outbox empty
        assert!(store.outbox_pending(10).await.unwrap().is_empty());

        // Log has 3 events
        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        assert_eq!(events.len(), 3);
    }

    #[tokio::test]
    async fn drain_redelivery_idempotent_via_log_event_id_dedup() {
        // Simulates relay crash between log.append and outbox_delete:
        // the outbox row is re-pending on next drain, log.append sees
        // the same event_id, dedups (Memory log uses ON CONFLICT
        // semantics by event_id at append time).
        let store = Arc::new(MemoryStore::new());
        let event_id = Uuid::new_v4();
        let row = make_row(event_id, 0);

        // First commit
        store.commit_reactor_batch(vec![row.clone()], None).await.unwrap();

        // Pre-write the same event_id directly to the log to simulate
        // "first drain succeeded at append, crashed before delete".
        let dup = NewEvent {
            event_id,
            parent_id: Some(row.source_event_id),
            correlation_id: row.correlation_id,
            event_type: row.event_type.clone(),
            payload: row.fact_payload.clone(),
            created_at: chrono::Utc::now(),
            aggregate_type: None,
            aggregate_id: None,
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        };
        EventLogBackend::append(store.as_ref(), dup).await.unwrap();

        // Now the relay drains. Row is still in outbox; log already
        // has the event. Append should be a no-op due to event_id
        // idempotency (per C1).
        let relay = RelayLoop::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );
        let n = relay.drain_once(10).await.unwrap();
        assert_eq!(n, 1);

        // Log has exactly one event with this event_id.
        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let matching: Vec<_> = events.iter().filter(|e| e.event_id == event_id).collect();
        assert_eq!(matching.len(), 1, "log dedups duplicate event_id");

        // Outbox row deleted.
        assert!(store.outbox_pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn drain_returns_0_when_outbox_empty() {
        let store = Arc::new(MemoryStore::new());
        let relay = RelayLoop::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );
        assert_eq!(relay.drain_once(10).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn drained_event_carries_reactor_origin_metadata() {
        let store = Arc::new(MemoryStore::new());
        let event_id = Uuid::new_v4();
        store.commit_reactor_batch(
            vec![make_row(event_id, 7)],
            None,
        ).await.unwrap();

        let relay = RelayLoop::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );
        relay.drain_once(10).await.unwrap();

        let events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let row = &events[0];
        assert_eq!(
            row.metadata.get("_reactor_id").and_then(|v| v.as_str()),
            Some("r.test"),
        );
        assert_eq!(
            row.metadata.get("_reactor_output_index").and_then(|v| v.as_u64()),
            Some(7),
        );
    }
}
