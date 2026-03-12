//! In-memory [`InspectorReadModel`] implementation for [`causal::MemoryStore`].
//!
//! Reads directly from MemoryStore's internal event log and reactor metadata.
//! Suitable for development, testing, and example applications.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use causal::{MemoryStore, ReactorQueue};

use crate::read_model::{
    InspectorReadModel, EventQuery, ReactorDescriptionEntry, ReactorLogEntry, ReactorOutcomeEntry,
    StoredEvent,
};

/// Convert a `PersistedEvent` to a `StoredEvent`.
fn to_stored(e: &causal::types::PersistedEvent) -> StoredEvent {
    StoredEvent {
        seq: e.position.raw() as i64,
        ts: e.created_at,
        event_type: e.event_type.clone(),
        payload: e.payload.clone(),
        id: Some(e.event_id),
        parent_id: e.parent_id,
        correlation_id: Some(e.correlation_id),
        reactor_id: e
            .metadata
            .get("reactor_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

#[async_trait]
impl InspectorReadModel for MemoryStore {
    async fn list_events(&self, query: &EventQuery) -> Result<Vec<StoredEvent>> {
        let log = self.global_log().lock();
        let limit = query.limit.min(200);

        let iter = log.iter().rev();

        let results: Vec<StoredEvent> = iter
            .filter(|e| {
                // Cursor filter
                if let Some(cursor) = query.cursor {
                    if (e.position.raw() as i64) >= cursor {
                        return false;
                    }
                }
                // Time range filters
                if let Some(ref from) = query.from {
                    if e.created_at < *from {
                        return false;
                    }
                }
                if let Some(ref to) = query.to {
                    if e.created_at > *to {
                        return false;
                    }
                }
                // Correlation ID filter
                if let Some(ref cid) = query.correlation_id {
                    if e.correlation_id.to_string() != *cid {
                        return false;
                    }
                }
                // Search filter
                if let Some(ref search) = query.search {
                    let search_lower = search.to_lowercase();
                    let payload_str = serde_json::to_string(&e.payload).unwrap_or_default();
                    let matches = e.event_type.to_lowercase().contains(&search_lower)
                        || payload_str.to_lowercase().contains(&search_lower)
                        || e.correlation_id
                            .to_string()
                            .to_lowercase()
                            .contains(&search_lower);
                    if !matches {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .map(to_stored)
            .collect();

        Ok(results)
    }

    async fn get_event(&self, seq: i64) -> Result<Option<StoredEvent>> {
        let log = self.global_log().lock();
        Ok(log
            .iter()
            .find(|e| e.position.raw() as i64 == seq)
            .map(to_stored))
    }

    async fn causal_tree(&self, seq: i64) -> Result<(Vec<StoredEvent>, i64)> {
        let log = self.global_log().lock();

        // Find the target event's correlation_id
        let correlation_id = log
            .iter()
            .find(|e| e.position.raw() as i64 == seq)
            .map(|e| e.correlation_id);

        let Some(cid) = correlation_id else {
            return Ok((vec![], seq));
        };

        let events: Vec<StoredEvent> = log
            .iter()
            .filter(|e| e.correlation_id == cid)
            .map(to_stored)
            .collect();

        let root_seq = events
            .iter()
            .find(|e| e.parent_id.is_none())
            .map(|e| e.seq)
            .unwrap_or(seq);

        Ok((events, root_seq))
    }

    async fn causal_flow(&self, correlation_id: &str) -> Result<Vec<StoredEvent>> {
        let Ok(cid) = Uuid::parse_str(correlation_id) else {
            return Ok(vec![]);
        };
        let log = self.global_log().lock();
        Ok(log
            .iter()
            .filter(|e| e.correlation_id == cid)
            .map(to_stored)
            .collect())
    }

    async fn events_from_seq(&self, start_seq: i64, limit: usize) -> Result<Vec<StoredEvent>> {
        let log = self.global_log().lock();
        let limit = limit.min(500);
        Ok(log
            .iter()
            .filter(|e| e.position.raw() as i64 >= start_seq)
            .take(limit)
            .map(to_stored)
            .collect())
    }

    async fn reactor_logs(
        &self,
        _event_id: Uuid,
        _reactor_id: &str,
    ) -> Result<Vec<ReactorLogEntry>> {
        Ok(vec![])
    }

    async fn reactor_logs_by_correlation(
        &self,
        _correlation_id: &str,
    ) -> Result<Vec<ReactorLogEntry>> {
        Ok(vec![])
    }

    async fn reactor_outcomes(&self, _correlation_id: &str) -> Result<Vec<ReactorOutcomeEntry>> {
        Ok(vec![])
    }

    async fn reactor_descriptions(
        &self,
        correlation_id: &str,
    ) -> Result<Vec<ReactorDescriptionEntry>> {
        let Ok(cid) = Uuid::parse_str(correlation_id) else {
            return Ok(vec![]);
        };

        let descriptions = ReactorQueue::get_descriptions(self, cid)
            .await
            .unwrap_or_default();

        Ok(descriptions
            .into_iter()
            .map(|(reactor_id, description)| ReactorDescriptionEntry {
                reactor_id,
                description,
            })
            .collect())
    }
}
