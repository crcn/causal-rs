//! In-memory [`InspectorReadModel`] implementation.
//!
//! Reads directly from MemoryStore's internal event log and reactor metadata.
//! Suitable for development, testing, and example applications.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use causal::effect_store::InMemoryEffectStore;
use causal::MemoryStore;

use crate::read_model::{
    AggregateKeyEntry, AggregateKeysPage, AggregateLifecycleEntry, AggregateStateSnapshotEntry,
    CorrelationSummaryEntry, EffectRecord, EventQuery, InspectorReadModel, ReactorAttemptEntry,
    ReactorDependencyEntry, ReactorDescriptionEntry, ReactorDescriptionSnapshotEntry,
    ReactorLogEntry, ReactorOutcomeEntry, StoredEvent, SubjectChainEventRaw, SubjectChainMode,
    SubjectChainPage, SubjectChainSourceMode,
};

/// Convert a `RecordedEvent` to a `StoredEvent`.
fn to_stored(e: &causal::types::RecordedEvent) -> StoredEvent {
    StoredEvent {
        seq: e.position.raw() as i64,
        ts: e.created_at,
        event_type: e.event_type.clone(),
        payload: e.payload.clone(),
        id: Some(e.event_id),
        causation_id: e.causation_id,
        workflow_id: Some(e.workflow_id),
        reactor_id: e
            .metadata
            .get("reactor_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        aggregate_type: Some(e.category.clone()),
        aggregate_id: Some(e.subject_id),
        stream_revision: Some(e.revision.raw()),
    }
}

/// In-memory [`InspectorReadModel`] backed by a [`MemoryStore`].
///
/// # Example
///
/// ```ignore
/// let store = Arc::new(MemoryStore::new());
/// schema_builder.data(Arc::new(MemoryInspectorReadModel::new(store)) as Arc<dyn InspectorReadModel>);
/// ```
pub struct MemoryInspectorReadModel {
    store: Arc<MemoryStore>,
    effects: Option<Arc<InMemoryEffectStore>>,
}

impl MemoryInspectorReadModel {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store, effects: None }
    }

    /// Attach an effect store for `effects_for_event` queries.
    pub fn with_effects(mut self, effects: Arc<InMemoryEffectStore>) -> Self {
        self.effects = Some(effects);
        self
    }
}

#[async_trait]
impl InspectorReadModel for MemoryInspectorReadModel {
    async fn list_events(&self, query: &EventQuery) -> Result<Vec<StoredEvent>> {
        let log = self.store.global_log();
        let limit = query.limit.min(200);

        let iter = log.iter().rev();

        let results: Vec<StoredEvent> = iter
            .filter(|e| {
                if let Some(cursor) = query.cursor {
                    if (e.position.raw() as i64) >= cursor {
                        return false;
                    }
                }
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
                if let Some(ref cid) = query.workflow_id {
                    if e.workflow_id.to_string() != *cid {
                        return false;
                    }
                }
                if let Some(ref key) = query.aggregate_key {
                    let event_key = format!("{}:{}", e.category, e.subject_id);
                    if event_key != *key {
                        return false;
                    }
                }
                if let Some(ref search) = query.search {
                    let search_lower = search.to_lowercase();
                    let payload_str = serde_json::to_string(&e.payload).unwrap_or_default();
                    let matches = e.event_type.to_lowercase().contains(&search_lower)
                        || payload_str.to_lowercase().contains(&search_lower)
                        || e.workflow_id
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
        let log = self.store.global_log();
        Ok(log
            .iter()
            .find(|e| e.position.raw() as i64 == seq)
            .map(to_stored))
    }

    async fn causal_tree(&self, seq: i64) -> Result<(Vec<StoredEvent>, i64)> {
        let log = self.store.global_log();

        let workflow_id = log
            .iter()
            .find(|e| e.position.raw() as i64 == seq)
            .map(|e| e.workflow_id);

        let Some(cid) = workflow_id else {
            return Ok((vec![], seq));
        };

        let events: Vec<StoredEvent> = log
            .iter()
            .filter(|e| e.workflow_id == cid)
            .map(to_stored)
            .collect();

        let root_seq = events
            .iter()
            .find(|e| e.causation_id.is_none())
            .map(|e| e.seq)
            .unwrap_or(seq);

        Ok((events, root_seq))
    }

    async fn causal_flow(&self, workflow_id: &str) -> Result<Vec<StoredEvent>> {
        let Ok(cid) = Uuid::parse_str(workflow_id) else {
            return Ok(vec![]);
        };
        let log = self.store.global_log();
        Ok(log
            .iter()
            .filter(|e| e.workflow_id == cid)
            .map(to_stored)
            .collect())
    }

    async fn events_from_seq(&self, start_seq: i64, limit: usize) -> Result<Vec<StoredEvent>> {
        let log = self.store.global_log();
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
        let logs = self.store.reactor_log_entries().lock();
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

    async fn reactor_logs_by_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorLogEntry>> {
        let Ok(cid) = Uuid::parse_str(workflow_id) else {
            return Ok(vec![]);
        };
        let event_ids: std::collections::HashSet<Uuid> = {
            let log = self.store.global_log();
            log.iter()
                .filter(|e| e.workflow_id == cid)
                .map(|e| e.event_id)
                .collect()
        };
        let logs = self.store.reactor_log_entries().lock();
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

    async fn reactor_outcomes(&self, workflow_id: &str) -> Result<Vec<ReactorOutcomeEntry>> {
        let Ok(cid) = Uuid::parse_str(workflow_id) else {
            return Ok(vec![]);
        };

        let mut by_reactor: std::collections::HashMap<
            String,
            (String, Option<String>, i32, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, Vec<String>),
        > = std::collections::HashMap::new();

        for entry in self.store.reactor_executions().iter() {
            let (event_id, reactor_id) = entry.key();
            let (corr_id, started_at, completed_at, status, error, attempts) = entry.value();
            if *corr_id != cid {
                continue;
            }

            let row = by_reactor.entry(reactor_id.clone()).or_insert_with(|| {
                (status.clone(), error.clone(), 0, None, None, Vec::new())
            });
            if status == "error" {
                row.0 = "error".to_string();
                row.1 = error.clone();
            }
            row.2 += attempts + 1;
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

        // Reactors that accepted a divergent redelivery in this workflow —
        // surfaced as a `diverged` flag, orthogonal to lifecycle status.
        let diverged_reactors: std::collections::HashSet<String> = self
            .store
            .reactor_divergences()
            .iter()
            .filter(|e| e.value().0 == cid)
            .map(|e| e.key().1.clone())
            .collect();

        Ok(by_reactor
            .into_iter()
            .map(|(reactor_id, (status, error, attempts, started_at, completed_at, triggering_event_ids))| {
                let diverged = diverged_reactors.contains(&reactor_id);
                ReactorOutcomeEntry {
                    reactor_id,
                    status,
                    error,
                    attempts: attempts as i64,
                    started_at,
                    completed_at,
                    triggering_event_ids,
                    diverged,
                }
            })
            .collect())
    }

    async fn reactor_attempt_history(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorAttemptEntry>> {
        let Ok(cid) = Uuid::parse_str(workflow_id) else {
            return Ok(vec![]);
        };
        let history = self.store.reactor_attempt_history().lock();
        let mut result: Vec<ReactorAttemptEntry> = history
            .iter()
            .filter(|(_, _, corr_id, _, _, _, _, _)| *corr_id == cid)
            .map(|(event_id, reactor_id, corr_id, attempt, status, error, started_at, completed_at)| {
                ReactorAttemptEntry {
                    event_id: *event_id,
                    reactor_id: reactor_id.clone(),
                    workflow_id: corr_id.to_string(),
                    attempt: *attempt,
                    status: status.clone(),
                    error: error.clone(),
                    started_at: *started_at,
                    completed_at: *completed_at,
                }
            })
            .collect();
        result.sort_by_key(|a| a.started_at);
        Ok(result)
    }

    async fn reactor_descriptions(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorDescriptionEntry>> {
        let Ok(cid) = Uuid::parse_str(workflow_id) else {
            return Ok(vec![]);
        };

        let snapshots = self.store.reactor_description_snapshots().lock();
        let mut latest: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        for (corr, _seq, _event_id, reactor_id, description) in snapshots.iter() {
            if *corr == cid {
                latest.insert(reactor_id.clone(), description.clone());
            }
        }

        Ok(latest
            .into_iter()
            .map(|(reactor_id, description)| ReactorDescriptionEntry {
                reactor_id,
                description,
            })
            .collect())
    }

    async fn reactor_description_snapshots(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorDescriptionSnapshotEntry>> {
        let Ok(cid) = Uuid::parse_str(workflow_id) else {
            return Ok(vec![]);
        };

        let snapshots = self.store.reactor_description_snapshots().lock();
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
        workflow_id: &str,
    ) -> Result<Vec<AggregateStateSnapshotEntry>> {
        let Ok(cid) = Uuid::parse_str(workflow_id) else {
            return Ok(vec![]);
        };

        let event_types: std::collections::HashMap<Uuid, String> = {
            let log = self.store.global_log();
            log.iter()
                .filter(|e| e.workflow_id == cid)
                .map(|e| (e.event_id, e.event_type.clone()))
                .collect()
        };

        let snapshots = self.store.aggregate_state_snapshots().lock();
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

    async fn list_workflows(
        &self,
        search: Option<&str>,
        limit: usize,
        cursor: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<CorrelationSummaryEntry>> {
        let log = self.store.global_log();

        let mut by_corr: std::collections::HashMap<
            Uuid,
            (i64, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, String),
        > = std::collections::HashMap::new();

        for e in log.iter() {
            if e.workflow_id.is_nil() {
                continue;
            }
            let entry = by_corr
                .entry(e.workflow_id)
                .or_insert_with(|| (0, e.created_at, e.created_at, String::new()));
            entry.0 += 1;
            if e.created_at < entry.1 {
                entry.1 = e.created_at;
            }
            if e.created_at > entry.2 {
                entry.2 = e.created_at;
            }
            if e.causation_id.is_none() && entry.3.is_empty() {
                entry.3 = e.event_type.clone();
            }
        }

        let error_workflows: std::collections::HashSet<Uuid> = self
            .store
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
                    workflow_id: cid.to_string(),
                    event_count: count,
                    first_ts,
                    last_ts,
                    root_event_type,
                    has_errors: error_workflows.contains(&cid),
                }
            })
            .collect();

        results.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));

        if let Some(cursor_ts) = cursor {
            results.retain(|r| r.last_ts < cursor_ts);
        }

        results.truncate(limit);

        Ok(results)
    }

    async fn reactor_dependencies(&self) -> Result<Vec<ReactorDependencyEntry>> {
        let log = self.store.global_log();

        let mut inputs: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
        let mut outputs: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();

        let event_type_by_id: std::collections::HashMap<Uuid, String> = log
            .iter()
            .filter_map(|e| Some((e.event_id, e.event_type.clone())))
            .collect();

        for entry in self.store.reactor_executions().iter() {
            let (event_id, reactor_id) = entry.key();
            if let Some(event_type) = event_type_by_id.get(event_id) {
                inputs
                    .entry(reactor_id.clone())
                    .or_default()
                    .insert(event_type.clone());
            }
        }

        for e in log.iter() {
            if let Some(rid) = e.metadata.get("reactor_id").and_then(|v| v.as_str()) {
                outputs
                    .entry(rid.to_string())
                    .or_default()
                    .insert(e.event_type.clone());
            }
        }

        let all_reactor_ids: std::collections::HashSet<String> = inputs
            .keys()
            .chain(outputs.keys())
            .cloned()
            .collect();

        let mut results: Vec<ReactorDependencyEntry> = all_reactor_ids
            .into_iter()
            .map(|reactor_id| {
                let mut input_types: Vec<String> = inputs
                    .remove(&reactor_id)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                input_types.sort();
                let mut output_types: Vec<String> = outputs
                    .remove(&reactor_id)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                output_types.sort();
                ReactorDependencyEntry {
                    reactor_id,
                    input_event_types: input_types,
                    output_event_types: output_types,
                }
            })
            .collect();
        results.sort_by(|a, b| a.reactor_id.cmp(&b.reactor_id));
        Ok(results)
    }

    async fn aggregate_lifecycle(
        &self,
        aggregate_key: &str,
        limit: usize,
    ) -> Result<Vec<AggregateLifecycleEntry>> {
        let snapshots = self.store.aggregate_state_snapshots().lock();

        let event_info: std::collections::HashMap<Uuid, (String, chrono::DateTime<chrono::Utc>)> = {
            let log = self.store.global_log();
            log.iter()
                .map(|e| (e.event_id, (e.event_type.clone(), e.created_at)))
                .collect()
        };

        let mut result: Vec<AggregateLifecycleEntry> = snapshots
            .iter()
            .filter(|(_, _, _, key, _)| key == aggregate_key)
            .filter_map(|(corr_id, seq, event_id, key, state)| {
                let (event_type, ts) = event_info.get(event_id)?;
                Some(AggregateLifecycleEntry {
                    seq: *seq as i64,
                    event_id: *event_id,
                    event_type: event_type.clone(),
                    ts: *ts,
                    workflow_id: corr_id.to_string(),
                    aggregate_key: key.clone(),
                    state: state.clone(),
                })
            })
            .collect();

        result.sort_by_key(|e| e.seq);
        result.truncate(limit);
        Ok(result)
    }

    async fn list_aggregate_keys(&self) -> Result<Vec<String>> {
        let snapshots = self.store.aggregate_state_snapshots().lock();
        let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_, _, _, key, _) in snapshots.iter() {
            keys.insert(key.clone());
        }
        let mut sorted: Vec<String> = keys.into_iter().collect();
        sorted.sort();
        Ok(sorted)
    }

    async fn effects_for_event(&self, event_id: Uuid) -> Result<Vec<EffectRecord>> {
        let Some(effects) = &self.effects else { return Ok(vec![]); };
        Ok(effects
            .scan_by_trigger(event_id)
            .into_iter()
            .map(|(k, v)| EffectRecord {
                consumer: k.consumer,
                label: k.label,
                value: v,
                created_at: chrono::Utc::now(),
            })
            .collect())
    }

    async fn list_aggregate_types(
        &self,
        search: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        let log = self.store.global_log();
        let mut types: std::collections::HashSet<String> =
            log.iter().map(|e| e.category.clone()).collect();
        if let Some(s) = search {
            let s = s.to_lowercase();
            types.retain(|t| t.to_lowercase().contains(&s));
        }
        let mut sorted: Vec<String> = types.into_iter().collect();
        sorted.sort();
        sorted.truncate(limit);
        Ok(sorted)
    }

    async fn list_aggregate_keys_by_type(
        &self,
        aggregate_type: &str,
        search: Option<&str>,
        limit: usize,
        cursor: Option<Uuid>,
    ) -> Result<AggregateKeysPage> {
        let log = self.store.global_log();
        // BTreeMap keeps aggregate_ids sorted, matching PG's ORDER BY aggregate_id.
        let mut first_by_entity: std::collections::BTreeMap<Uuid, (String, serde_json::Value)> =
            std::collections::BTreeMap::new();
        for e in log.iter() {
            if e.category == aggregate_type {
                first_by_entity
                    .entry(e.subject_id)
                    .or_insert_with(|| (e.event_type.clone(), e.payload.clone()));
            }
        }

        let search_lower = search.map(|s| s.to_lowercase());
        let mut entries: Vec<AggregateKeyEntry> = first_by_entity
            .into_iter()
            .filter(|(id, _)| cursor.map_or(true, |c| *id > c))
            .filter(|(id, _)| {
                search_lower
                    .as_ref()
                    .map_or(true, |s| id.to_string().to_lowercase().contains(s))
            })
            .take(limit + 1)
            .map(|(id, (event_type, first_payload))| AggregateKeyEntry {
                aggregate_id: id,
                event_type,
                first_payload,
            })
            .collect();

        let next_cursor = if entries.len() > limit {
            entries.truncate(limit);
            entries.last().map(|e| e.aggregate_id)
        } else {
            None
        };

        Ok(AggregateKeysPage { entries, next_cursor })
    }

    async fn subject_chain(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        mode: SubjectChainMode,
        limit: usize,
        cursor: Option<i64>,
    ) -> Result<SubjectChainPage> {
        use std::collections::{BTreeMap, HashMap, HashSet};

        let log = self.store.global_log();

        // Stream events: belong to (aggregate_type, aggregate_id), after cursor.
        let stream_events: Vec<SubjectChainEventRaw> = log
            .iter()
            .filter(|e| e.category == aggregate_type && e.subject_id == aggregate_id)
            .filter(|e| cursor.map_or(true, |c| e.position.raw() as i64 > c))
            .map(|e| SubjectChainEventRaw {
                stored: to_stored(e),
                source_mode: SubjectChainSourceMode::Stream,
            })
            .collect();

        if mode == SubjectChainMode::Stream {
            let events: Vec<SubjectChainEventRaw> =
                stream_events.into_iter().take(limit).collect();
            let next_cursor = events.last().map(|e| e.stored.seq);
            return Ok(SubjectChainPage { events, next_cursor, depth_cap_reached: false });
        }

        // BFS to find all descendant event IDs.
        let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for e in log.iter() {
            if let Some(cid) = e.causation_id {
                children.entry(cid).or_default().push(e.event_id);
            }
        }

        let origins: Vec<Uuid> = log
            .iter()
            .filter(|e| e.category == aggregate_type && e.subject_id == aggregate_id)
            .map(|e| e.event_id)
            .collect();

        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut frontier: Vec<(Uuid, u8)> = origins
            .iter()
            .flat_map(|oid| children.get(oid).into_iter().flatten().map(|&c| (c, 1u8)))
            .collect();
        let mut depth_cap_reached = false;

        while let Some((eid, depth)) = frontier.pop() {
            if !visited.insert(eid) {
                continue;
            }
            if depth < 10 {
                if let Some(cs) = children.get(&eid) {
                    frontier.extend(cs.iter().map(|&c| (c, depth + 1)));
                }
            } else {
                depth_cap_reached = true;
            }
        }

        let desc_events: Vec<SubjectChainEventRaw> = log
            .iter()
            .filter(|e| visited.contains(&e.event_id))
            .filter(|e| cursor.map_or(true, |c| e.position.raw() as i64 > c))
            .map(|e| SubjectChainEventRaw {
                stored: to_stored(e),
                source_mode: SubjectChainSourceMode::Descendant,
            })
            .collect();

        match mode {
            SubjectChainMode::Descendants => {
                let mut sorted = desc_events;
                sorted.sort_by_key(|e| e.stored.seq);
                let events: Vec<_> = sorted.into_iter().take(limit).collect();
                let next_cursor = events.last().map(|e| e.stored.seq);
                Ok(SubjectChainPage { events, next_cursor, depth_cap_reached })
            }
            SubjectChainMode::Both => {
                // Merge by position — stream wins over descendant for same seq.
                let mut merged: BTreeMap<i64, SubjectChainEventRaw> = BTreeMap::new();
                for ev in desc_events {
                    merged.insert(ev.stored.seq, ev);
                }
                for ev in stream_events {
                    merged.insert(ev.stored.seq, ev);
                }
                let events: Vec<_> = merged.into_values().take(limit).collect();
                let next_cursor = events.last().map(|e| e.stored.seq);
                Ok(SubjectChainPage { events, next_cursor, depth_cap_reached })
            }
            SubjectChainMode::Stream => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use causal::effect_store::EffectStore;
    use causal::event_log::EventLogBackend;
    use causal::types::{EventData, StreamState};
    use uuid::Uuid;

    fn mk_event(
        category: &str,
        subject_id: Uuid,
        event_id: Uuid,
        workflow_id: Uuid,
        causation_id: Option<Uuid>,
        payload: serde_json::Value,
    ) -> EventData {
        EventData {
            event_id,
            causation_id,
            workflow_id,
            event_type: format!("{}_happened", category),
            payload,
            created_at: chrono::Utc::now(),
            category: Some(category.to_string()),
            subject_id: Some(subject_id),
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        }
    }

    async fn append(store: &causal::MemoryStore, category: &str, subject_id: Uuid, event_id: Uuid, workflow_id: Uuid, causation_id: Option<Uuid>, payload: serde_json::Value) {
        store
            .append_to_stream(category, subject_id, StreamState::Any, vec![mk_event(category, subject_id, event_id, workflow_id, causation_id, payload)])
            .await
            .unwrap();
    }

    // ── reactor_outcomes: divergence flag ────────────────────────────────────

    #[tokio::test]
    async fn reactor_outcomes_surfaces_divergence_orthogonal_to_status() {
        use causal::ReactorObserver;
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let now = chrono::Utc::now();

        // r.nd: reacted, completed, then its redelivery diverged (accepted).
        let trig_nd = Uuid::new_v4();
        store.reactor_started(trig_nd, "r.nd", wf, 1, now);
        store.reactor_completed(trig_nd, "r.nd", wf, 1, now, now, &[]);
        store.reactor_divergence(trig_nd, "r.nd", wf, "payload at `nonce`");

        // r.clean: completed, no divergence.
        let trig_clean = Uuid::new_v4();
        store.reactor_started(trig_clean, "r.clean", wf, 1, now);
        store.reactor_completed(trig_clean, "r.clean", wf, 1, now, now, &[]);

        let model = MemoryInspectorReadModel::new(store);
        let outcomes = model.reactor_outcomes(&wf.to_string()).await.unwrap();

        let nd = outcomes.iter().find(|o| o.reactor_id == "r.nd").expect("r.nd present");
        assert!(nd.diverged, "the divergent reactor is flagged");
        assert_eq!(
            nd.status, "completed",
            "divergence is orthogonal to status — react() completed, it is NOT an error",
        );
        assert!(nd.error.is_none(), "divergence is not surfaced as an error");

        let clean = outcomes.iter().find(|o| o.reactor_id == "r.clean").expect("r.clean present");
        assert!(!clean.diverged, "a non-divergent reactor is not flagged");
        assert_eq!(clean.status, "completed");
    }

    // ── list_aggregate_types ─────────────────────────────────────────────────

    #[tokio::test]
    async fn list_aggregate_types_returns_distinct_sorted() {
        let store = Arc::new(causal::MemoryStore::new());
        let id = Uuid::new_v4();
        let wf = Uuid::new_v4();
        append(&store, "source", id, Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        append(&store, "source", id, Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        append(&store, "actor", Uuid::new_v4(), Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        append(&store, "signal", Uuid::new_v4(), Uuid::new_v4(), wf, None, serde_json::json!({})).await;

        let model = MemoryInspectorReadModel::new(store);
        let types = model.list_aggregate_types(None, 100).await.unwrap();
        assert_eq!(types, vec!["actor", "signal", "source"]);
    }

    #[tokio::test]
    async fn list_aggregate_types_search_filters() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        append(&store, "source", Uuid::new_v4(), Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        append(&store, "actor", Uuid::new_v4(), Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        append(&store, "signal", Uuid::new_v4(), Uuid::new_v4(), wf, None, serde_json::json!({})).await;

        let model = MemoryInspectorReadModel::new(store);
        let types = model.list_aggregate_types(Some("sou"), 100).await.unwrap();
        assert_eq!(types, vec!["source"]);
    }

    #[tokio::test]
    async fn list_aggregate_types_respects_limit() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        for cat in ["aaa", "bbb", "ccc", "ddd"] {
            append(&store, cat, Uuid::new_v4(), Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        }

        let model = MemoryInspectorReadModel::new(store);
        let types = model.list_aggregate_types(None, 2).await.unwrap();
        assert_eq!(types.len(), 2);
        assert_eq!(types, vec!["aaa", "bbb"]);
    }

    // ── list_aggregate_keys_by_type ──────────────────────────────────────────

    #[tokio::test]
    async fn list_aggregate_keys_by_type_returns_first_event_per_entity() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let source_a = Uuid::new_v4();
        let source_b = Uuid::new_v4();

        // Two events for source_a, one for source_b, one different type.
        append(&store, "source", source_a, Uuid::new_v4(), wf, None, serde_json::json!({"name": "a-first"})).await;
        append(&store, "source", source_a, Uuid::new_v4(), wf, None, serde_json::json!({"name": "a-second"})).await;
        append(&store, "source", source_b, Uuid::new_v4(), wf, None, serde_json::json!({"name": "b-first"})).await;
        append(&store, "actor", Uuid::new_v4(), Uuid::new_v4(), wf, None, serde_json::json!({})).await;

        let model = MemoryInspectorReadModel::new(store);
        let page = model.list_aggregate_keys_by_type("source", None, 100, None).await.unwrap();
        assert_eq!(page.entries.len(), 2);

        // Both source entities present; verify first payload was used.
        let entry_a = page.entries.iter().find(|e| e.aggregate_id == source_a).unwrap();
        assert_eq!(entry_a.first_payload["name"], "a-first");

        let entry_b = page.entries.iter().find(|e| e.aggregate_id == source_b).unwrap();
        assert_eq!(entry_b.first_payload["name"], "b-first");

        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_aggregate_keys_by_type_cursor_pagination() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        // Create 3 sources with deterministic UUIDs (sorted by UUID value).
        let ids: Vec<Uuid> = (0u8..3).map(|i| {
            let mut bytes = [0u8; 16];
            bytes[15] = i + 1;
            Uuid::from_bytes(bytes)
        }).collect();

        for id in &ids {
            append(&store, "source", *id, Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        }

        let model = MemoryInspectorReadModel::new(store);

        // Page 1: limit=2 → 2 entries + next_cursor.
        let page1 = model.list_aggregate_keys_by_type("source", None, 2, None).await.unwrap();
        assert_eq!(page1.entries.len(), 2);
        assert!(page1.next_cursor.is_some());

        // Page 2: continue from cursor → 1 entry, no next_cursor.
        let page2 = model.list_aggregate_keys_by_type("source", None, 2, page1.next_cursor).await.unwrap();
        assert_eq!(page2.entries.len(), 1);
        assert!(page2.next_cursor.is_none());

        // Pages together cover all 3.
        let all_ids: std::collections::HashSet<Uuid> = page1.entries.iter().chain(page2.entries.iter()).map(|e| e.aggregate_id).collect();
        assert_eq!(all_ids.len(), 3);
    }

    // ── subject_chain — Stream mode ──────────────────────────────────────────

    #[tokio::test]
    async fn subject_chain_stream_returns_own_events_ordered_by_seq() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let other = Uuid::new_v4();

        append(&store, "source", subject, Uuid::new_v4(), wf, None, serde_json::json!({"n": 1})).await;
        append(&store, "source", other,   Uuid::new_v4(), wf, None, serde_json::json!({})).await;
        append(&store, "source", subject, Uuid::new_v4(), wf, None, serde_json::json!({"n": 2})).await;

        let model = MemoryInspectorReadModel::new(store);
        let page = model.subject_chain("source", subject, SubjectChainMode::Stream, 100, None).await.unwrap();

        assert_eq!(page.events.len(), 2);
        assert!(page.events[0].stored.seq < page.events[1].stored.seq);
        assert!(page.events.iter().all(|e| e.source_mode == SubjectChainSourceMode::Stream));
        assert!(!page.depth_cap_reached);
    }

    #[tokio::test]
    async fn subject_chain_stream_cursor_excludes_earlier_events() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let subject = Uuid::new_v4();

        append(&store, "source", subject, Uuid::new_v4(), wf, None, serde_json::json!({"n": 1})).await;
        append(&store, "source", subject, Uuid::new_v4(), wf, None, serde_json::json!({"n": 2})).await;
        append(&store, "source", subject, Uuid::new_v4(), wf, None, serde_json::json!({"n": 3})).await;

        let model = MemoryInspectorReadModel::new(store);
        let first_page = model.subject_chain("source", subject, SubjectChainMode::Stream, 1, None).await.unwrap();
        assert_eq!(first_page.events.len(), 1);

        let cursor = first_page.next_cursor;
        let second_page = model.subject_chain("source", subject, SubjectChainMode::Stream, 100, cursor).await.unwrap();
        assert_eq!(second_page.events.len(), 2);
    }

    // ── subject_chain — Descendants mode ────────────────────────────────────

    #[tokio::test]
    async fn subject_chain_descendants_bfs_finds_children() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let subject = Uuid::new_v4();

        // origin → child1 → grandchild
        let origin_id = Uuid::new_v4();
        let child1_id = Uuid::new_v4();
        let grandchild_id = Uuid::new_v4();

        append(&store, "source", subject, origin_id, wf, None, serde_json::json!({})).await;
        append(&store, "task", Uuid::new_v4(), child1_id, wf, Some(origin_id), serde_json::json!({})).await;
        append(&store, "task", Uuid::new_v4(), grandchild_id, wf, Some(child1_id), serde_json::json!({})).await;

        let model = MemoryInspectorReadModel::new(store);
        let page = model.subject_chain("source", subject, SubjectChainMode::Descendants, 100, None).await.unwrap();

        assert_eq!(page.events.len(), 2);
        let ids: std::collections::HashSet<Uuid> = page.events.iter().filter_map(|e| e.stored.id).collect();
        assert!(ids.contains(&child1_id));
        assert!(ids.contains(&grandchild_id));
        assert!(page.events.iter().all(|e| e.source_mode == SubjectChainSourceMode::Descendant));
        assert!(!page.depth_cap_reached);
    }

    #[tokio::test]
    async fn subject_chain_descendants_depth_cap_at_10() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let subject = Uuid::new_v4();

        // Build a chain 10 levels deep from the origin.
        let origin_id = Uuid::new_v4();
        append(&store, "source", subject, origin_id, wf, None, serde_json::json!({})).await;

        let mut parent_id = origin_id;
        for _ in 0..10 {
            let child_id = Uuid::new_v4();
            append(&store, "task", Uuid::new_v4(), child_id, wf, Some(parent_id), serde_json::json!({})).await;
            parent_id = child_id;
        }

        let model = MemoryInspectorReadModel::new(store);
        let page = model.subject_chain("source", subject, SubjectChainMode::Descendants, 100, None).await.unwrap();

        assert!(page.depth_cap_reached, "10-level chain should trigger depth cap");
    }

    #[tokio::test]
    async fn subject_chain_descendants_no_depth_cap_for_shallow_tree() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let subject = Uuid::new_v4();

        let origin_id = Uuid::new_v4();
        append(&store, "source", subject, origin_id, wf, None, serde_json::json!({})).await;

        let mut parent_id = origin_id;
        for _ in 0..9 {
            let child_id = Uuid::new_v4();
            append(&store, "task", Uuid::new_v4(), child_id, wf, Some(parent_id), serde_json::json!({})).await;
            parent_id = child_id;
        }

        let model = MemoryInspectorReadModel::new(store);
        let page = model.subject_chain("source", subject, SubjectChainMode::Descendants, 100, None).await.unwrap();

        assert!(!page.depth_cap_reached, "9-level chain should not trigger depth cap");
        assert_eq!(page.events.len(), 9);
    }

    // ── subject_chain — Both mode ────────────────────────────────────────────

    #[tokio::test]
    async fn subject_chain_both_merges_stream_wins_on_overlap() {
        let store = Arc::new(causal::MemoryStore::new());
        let wf = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let other = Uuid::new_v4();

        // origin is in the stream (subject) AND causes a child.
        let origin_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        // An event emitted by a reactor for subject — in stream AND reachable as descendant of origin.
        let stream_and_desc_id = Uuid::new_v4();

        append(&store, "source", subject, origin_id, wf, None, serde_json::json!({})).await;
        append(&store, "task", other, child_id, wf, Some(origin_id), serde_json::json!({})).await;
        // This event is a descendant of child_id but also in the subject stream.
        append(&store, "source", subject, stream_and_desc_id, wf, Some(child_id), serde_json::json!({})).await;

        let model = MemoryInspectorReadModel::new(store);
        let page = model.subject_chain("source", subject, SubjectChainMode::Both, 100, None).await.unwrap();

        // origin, child, stream_and_desc — 3 unique events.
        assert_eq!(page.events.len(), 3);

        // stream_and_desc_id: appears once, as Stream (stream wins over descendant).
        let overlap = page.events.iter().find(|e| e.stored.id == Some(stream_and_desc_id)).unwrap();
        assert_eq!(overlap.source_mode, SubjectChainSourceMode::Stream);

        // child_id: only a descendant.
        let desc_only = page.events.iter().find(|e| e.stored.id == Some(child_id)).unwrap();
        assert_eq!(desc_only.source_mode, SubjectChainSourceMode::Descendant);

        // Ordered by seq ascending.
        let seqs: Vec<i64> = page.events.iter().map(|e| e.stored.seq).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }

    // ── effects_for_event ────────────────────────────────────────────────────

    #[tokio::test]
    async fn effects_for_event_without_store_returns_empty() {
        let store = Arc::new(causal::MemoryStore::new());
        let model = MemoryInspectorReadModel::new(store);
        let effects = model.effects_for_event(Uuid::new_v4()).await.unwrap();
        assert!(effects.is_empty());
    }

    #[tokio::test]
    async fn effects_for_event_returns_matching_effects() {
        let effect_store = Arc::new(causal::effect_store::InMemoryEffectStore::new());
        let trigger_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();

        let key1 = causal::effect_store::EffectKey::new("reactor.fetch", trigger_id, "html");
        let key2 = causal::effect_store::EffectKey::new("reactor.fetch", trigger_id, "meta");
        let key_other = causal::effect_store::EffectKey::new("reactor.fetch", other_id, "html");

        effect_store.put(&key1, serde_json::json!("<html>")).await.unwrap();
        effect_store.put(&key2, serde_json::json!({"title": "test"})).await.unwrap();
        effect_store.put(&key_other, serde_json::json!("other")).await.unwrap();

        let store = Arc::new(causal::MemoryStore::new());
        let model = MemoryInspectorReadModel::new(store).with_effects(effect_store);

        let effects = model.effects_for_event(trigger_id).await.unwrap();
        assert_eq!(effects.len(), 2);

        let labels: std::collections::HashSet<&str> = effects.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains("html"));
        assert!(labels.contains("meta"));
        assert!(effects.iter().all(|e| e.consumer == "reactor.fetch"));
    }

    #[tokio::test]
    async fn effects_for_event_no_effects_for_event_returns_empty() {
        let effect_store = Arc::new(causal::effect_store::InMemoryEffectStore::new());
        let trigger_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();

        let key = causal::effect_store::EffectKey::new("reactor.fetch", other_id, "html");
        effect_store.put(&key, serde_json::json!("<html>")).await.unwrap();

        let store = Arc::new(causal::MemoryStore::new());
        let model = MemoryInspectorReadModel::new(store).with_effects(effect_store);

        let effects = model.effects_for_event(trigger_id).await.unwrap();
        assert!(effects.is_empty());
    }
}
