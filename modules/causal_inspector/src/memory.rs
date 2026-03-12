//! In-memory [`InspectorReadModel`] implementation for [`causal::MemoryStore`].
//!
//! Reads directly from MemoryStore's internal event log and reactor metadata.
//! Suitable for development, testing, and example applications.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use causal::{MemoryStore, ReactorQueue};

use crate::read_model::{
    InspectorReadModel, EventQuery, AggregateStateSnapshotEntry,
    CorrelationSummaryEntry,
    ReactorDescriptionEntry, ReactorDescriptionSnapshotEntry,
    ReactorLogEntry, ReactorOutcomeEntry, StoredEvent,
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
        event_id: Uuid,
        reactor_id: &str,
    ) -> Result<Vec<ReactorLogEntry>> {
        let logs = self.reactor_log_entries().lock();
        Ok(logs
            .iter()
            .filter(|(eid, rid, _)| *eid == event_id && rid == reactor_id)
            .map(|(eid, rid, entry)| ReactorLogEntry {
                event_id: *eid,
                reactor_id: rid.clone(),
                level: entry.level.to_string().to_lowercase(),
                message: entry.message.clone(),
                data: entry.data.clone(),
                logged_at: entry.timestamp,
            })
            .collect())
    }

    async fn reactor_logs_by_correlation(
        &self,
        correlation_id: &str,
    ) -> Result<Vec<ReactorLogEntry>> {
        let Ok(cid) = Uuid::parse_str(correlation_id) else {
            return Ok(vec![]);
        };
        // Build set of event IDs in this correlation
        let event_ids: std::collections::HashSet<Uuid> = {
            let log = self.global_log().lock();
            log.iter()
                .filter(|e| e.correlation_id == cid)
                .map(|e| e.event_id)
                .collect()
        };
        let logs = self.reactor_log_entries().lock();
        Ok(logs
            .iter()
            .filter(|(eid, _, _)| event_ids.contains(eid))
            .map(|(eid, rid, entry)| ReactorLogEntry {
                event_id: *eid,
                reactor_id: rid.clone(),
                level: entry.level.to_string().to_lowercase(),
                message: entry.message.clone(),
                data: entry.data.clone(),
                logged_at: entry.timestamp,
            })
            .collect())
    }

    async fn reactor_outcomes(&self, correlation_id: &str) -> Result<Vec<ReactorOutcomeEntry>> {
        let Ok(cid) = Uuid::parse_str(correlation_id) else {
            return Ok(vec![]);
        };

        // Group executions by reactor_id for this correlation
        let mut by_reactor: std::collections::HashMap<
            String,
            (String, Option<String>, i32, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, Vec<String>),
        > = std::collections::HashMap::new();

        for entry in self.reactor_executions().iter() {
            let (event_id, reactor_id) = entry.key();
            let (corr_id, started_at, completed_at, status, error, attempts) = entry.value();
            if *corr_id != cid {
                continue;
            }

            let row = by_reactor.entry(reactor_id.clone()).or_insert_with(|| {
                (status.clone(), error.clone(), 0, None, None, Vec::new())
            });
            // Aggregate: worst status wins, sum attempts, min started_at, max completed_at
            if status == "error" {
                row.0 = "error".to_string();
                row.1 = error.clone();
            }
            row.2 += attempts + 1; // attempts is 0-based retry count
            match row.3 {
                Some(existing) if *started_at < existing => row.3 = Some(*started_at),
                None => row.3 = Some(*started_at),
                _ => {}
            }
            if let Some(ca) = completed_at {
                match row.4 {
                    Some(existing) if *ca > existing => row.4 = Some(*ca),
                    None => row.4 = Some(*ca),
                    _ => {}
                }
            }
            row.5.push(event_id.to_string());
        }

        Ok(by_reactor
            .into_iter()
            .map(|(reactor_id, (status, error, attempts, started_at, completed_at, triggering_event_ids))| {
                ReactorOutcomeEntry {
                    reactor_id,
                    status,
                    error,
                    attempts: attempts as i64,
                    started_at,
                    completed_at,
                    triggering_event_ids,
                }
            })
            .collect())
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

    async fn reactor_description_snapshots(
        &self,
        correlation_id: &str,
    ) -> Result<Vec<ReactorDescriptionSnapshotEntry>> {
        let Ok(cid) = Uuid::parse_str(correlation_id) else {
            return Ok(vec![]);
        };

        let snapshots = self.reactor_description_snapshots().lock();
        let mut result: Vec<ReactorDescriptionSnapshotEntry> = snapshots
            .iter()
            .filter(|(corr_id, _, _, _, _)| *corr_id == cid)
            .map(|(_, seq, event_id, reactor_id, description)| {
                ReactorDescriptionSnapshotEntry {
                    seq: *seq as i64,
                    event_id: *event_id,
                    reactor_id: reactor_id.clone(),
                    description: description.clone(),
                }
            })
            .collect();

        result.sort_by_key(|s| s.seq);
        Ok(result)
    }

    async fn aggregate_state_timeline(
        &self,
        correlation_id: &str,
    ) -> Result<Vec<AggregateStateSnapshotEntry>> {
        let Ok(cid) = Uuid::parse_str(correlation_id) else {
            return Ok(vec![]);
        };

        // Build event_id → event_type lookup from global log
        let event_types: std::collections::HashMap<Uuid, String> = {
            let log = self.global_log().lock();
            log.iter()
                .filter(|e| e.correlation_id == cid)
                .map(|e| (e.event_id, e.event_type.clone()))
                .collect()
        };

        let snapshots = self.aggregate_state_snapshots().lock();
        let mut result: Vec<AggregateStateSnapshotEntry> = snapshots
            .iter()
            .filter(|(corr_id, _, _, _, _)| *corr_id == cid)
            .map(|(_, seq, event_id, aggregate_key, state)| {
                AggregateStateSnapshotEntry {
                    seq: *seq as i64,
                    event_id: *event_id,
                    event_type: event_types
                        .get(event_id)
                        .cloned()
                        .unwrap_or_default(),
                    aggregate_key: aggregate_key.clone(),
                    state: state.clone(),
                }
            })
            .collect();

        result.sort_by_key(|s| s.seq);
        Ok(result)
    }

    async fn list_correlations(
        &self,
        search: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CorrelationSummaryEntry>> {
        let log = self.global_log().lock();

        // Group events by correlation_id
        let mut by_corr: std::collections::HashMap<
            Uuid,
            (i64, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, String),
        > = std::collections::HashMap::new();

        for e in log.iter() {
            let entry = by_corr
                .entry(e.correlation_id)
                .or_insert_with(|| (0, e.created_at, e.created_at, String::new()));
            entry.0 += 1;
            if e.created_at < entry.1 {
                entry.1 = e.created_at;
            }
            if e.created_at > entry.2 {
                entry.2 = e.created_at;
            }
            // Root event = no parent_id
            if e.parent_id.is_none() && entry.3.is_empty() {
                entry.3 = e.event_type.clone();
            }
        }

        // Check for errors via reactor_executions
        let error_correlations: std::collections::HashSet<Uuid> = self
            .reactor_executions()
            .iter()
            .filter(|entry| {
                let (_corr_id, _started_at, _completed_at, status, _error, _attempts) = entry.value();
                status == "error"
            })
            .map(|entry| {
                let (_corr_id, _started_at, _completed_at, _status, _error, _attempts) = entry.value();
                *_corr_id
            })
            .collect();

        let search_lower = search.map(|s| s.to_lowercase());

        let mut results: Vec<CorrelationSummaryEntry> = by_corr
            .into_iter()
            .filter(|(cid, (_, _, _, root_type))| {
                if let Some(ref s) = search_lower {
                    cid.to_string().to_lowercase().contains(s)
                        || root_type.to_lowercase().contains(s)
                } else {
                    true
                }
            })
            .map(|(cid, (count, first_ts, last_ts, root_event_type))| {
                CorrelationSummaryEntry {
                    correlation_id: cid.to_string(),
                    event_count: count,
                    first_ts,
                    last_ts,
                    root_event_type,
                    has_errors: error_correlations.contains(&cid),
                }
            })
            .collect();

        // Sort by last_ts descending (most recent first)
        results.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
        results.truncate(limit);

        Ok(results)
    }
}
