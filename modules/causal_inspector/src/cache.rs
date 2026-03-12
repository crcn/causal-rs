use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use crate::types::InspectorEvent;

const DEFAULT_CAPACITY: usize = 500_000;

/// Bounded in-memory cache of recent events for fast inspector panel queries.
/// Stores pre-computed `InspectorEvent` values with side-indexes for O(1) lookups.
pub struct EventCache {
    events: VecDeque<Arc<InspectorEvent>>,
    by_seq: HashMap<i64, Arc<InspectorEvent>>,
    by_correlation: HashMap<Uuid, Vec<i64>>,
    by_handler: HashMap<String, Vec<i64>>,
    capacity: usize,
}

impl EventCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity.min(DEFAULT_CAPACITY)),
            by_seq: HashMap::new(),
            by_correlation: HashMap::new(),
            by_handler: HashMap::new(),
            capacity,
        }
    }

    /// Hydrate from Postgres — loads the most recent N events.
    #[cfg(feature = "postgres")]
    pub async fn hydrate(
        pool: &sqlx::PgPool,
        capacity: usize,
        display: &dyn crate::display::EventDisplay,
    ) -> anyhow::Result<Self> {
        let start = std::time::Instant::now();

        let stored = crate::queries::get_events_from_seq(pool, 0, capacity as i64).await?;

        let mut cache = Self::new(capacity);
        for event in stored {
            let evt = Arc::new(event.to_inspector_event(display));
            cache.push_unchecked(evt);
        }

        let elapsed = start.elapsed();
        info!(
            events = cache.events.len(),
            elapsed_ms = elapsed.as_millis(),
            "Event cache hydrated"
        );

        Ok(cache)
    }

    /// Push a new event into the cache. Evicts the oldest if at capacity.
    pub fn push(&mut self, event: Arc<InspectorEvent>) {
        if self.events.len() >= self.capacity {
            self.evict_oldest();
        }
        self.push_unchecked(event);
    }

    fn push_unchecked(&mut self, event: Arc<InspectorEvent>) {
        let seq = event.seq;

        self.by_seq.insert(seq, Arc::clone(&event));

        if let Some(cid) = event.correlation_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()) {
            self.by_correlation.entry(cid).or_default().push(seq);
        }

        if let Some(ref reactor_id) = event.reactor_id {
            self.by_handler.entry(reactor_id.clone()).or_default().push(seq);
        }

        self.events.push_back(event);
    }

    fn evict_oldest(&mut self) {
        let Some(evicted) = self.events.pop_front() else {
            return;
        };

        let seq = evicted.seq;
        self.by_seq.remove(&seq);

        if let Some(cid) = evicted.correlation_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()) {
            if let Some(bucket) = self.by_correlation.get_mut(&cid) {
                if let Ok(pos) = bucket.binary_search(&seq) {
                    bucket.remove(pos);
                }
                if bucket.is_empty() {
                    self.by_correlation.remove(&cid);
                }
            }
        }

        if let Some(ref reactor_id) = evicted.reactor_id {
            if let Some(bucket) = self.by_handler.get_mut(reactor_id) {
                if let Ok(pos) = bucket.binary_search(&seq) {
                    bucket.remove(pos);
                }
                if bucket.is_empty() {
                    self.by_handler.remove(reactor_id);
                }
            }
        }
    }

    /// Get a single event by seq.
    pub fn get_by_seq(&self, seq: i64) -> Option<Arc<InspectorEvent>> {
        self.by_seq.get(&seq).cloned()
    }

    /// Paginated reverse-chronological event listing with optional filters.
    pub fn search(
        &self,
        term: Option<&str>,
        cursor: Option<i64>,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        correlation_id: Option<&str>,
        limit: usize,
    ) -> (Vec<Arc<InspectorEvent>>, Option<i64>) {
        let term_lower = term.map(|t| t.to_lowercase());

        let correlation_seqs: Option<&Vec<i64>> = correlation_id
            .and_then(|cid| Uuid::parse_str(cid).ok())
            .and_then(|uuid| self.by_correlation.get(&uuid));

        let mut results = Vec::with_capacity(limit);

        let iter: Box<dyn Iterator<Item = &Arc<InspectorEvent>>> = if let Some(seqs) = correlation_seqs {
            Box::new(seqs.iter().rev().filter_map(|s| self.by_seq.get(s)))
        } else {
            Box::new(self.events.iter().rev())
        };

        for event in iter {
            if let Some(c) = cursor {
                if event.seq >= c {
                    continue;
                }
            }

            if let Some(ref f) = from {
                if event.ts < *f {
                    continue;
                }
            }
            if let Some(ref t) = to {
                if event.ts > *t {
                    continue;
                }
            }

            if let Some(ref needle) = term_lower {
                let matches = event.payload.to_lowercase().contains(needle)
                    || event.event_type.to_lowercase().contains(needle)
                    || event.correlation_id.as_deref().map(|s| s.to_lowercase().contains(needle)).unwrap_or(false);

                if !matches {
                    continue;
                }
            }

            results.push(Arc::clone(event));
            if results.len() >= limit {
                break;
            }
        }

        let next_cursor = if results.len() >= limit {
            results.last().map(|e| e.seq)
        } else {
            None
        };

        (results, next_cursor)
    }

    /// Get all events sharing the same correlation_id as the given event.
    pub fn causal_tree(&self, seq: i64) -> Option<(Vec<Arc<InspectorEvent>>, i64)> {
        let event = self.by_seq.get(&seq)?;
        let cid_str = event.correlation_id.as_deref()?;
        let cid = Uuid::parse_str(cid_str).ok()?;

        let seqs = self.by_correlation.get(&cid)?;
        let mut events: Vec<Arc<InspectorEvent>> = seqs
            .iter()
            .filter_map(|s| self.by_seq.get(s).cloned())
            .collect();
        events.sort_by_key(|e| e.seq);

        let root_seq = events
            .iter()
            .find(|e| e.parent_id.is_none())
            .map(|e| e.seq)
            .unwrap_or(seq);

        Some((events, root_seq))
    }

    /// Get all events for a correlation_id, ordered by seq ascending.
    pub fn causal_flow(&self, correlation_id: &str) -> Option<Vec<Arc<InspectorEvent>>> {
        let cid = Uuid::parse_str(correlation_id).ok()?;
        let seqs = self.by_correlation.get(&cid)?;
        let mut events: Vec<Arc<InspectorEvent>> = seqs
            .iter()
            .filter_map(|s| self.by_seq.get(s).cloned())
            .collect();
        events.sort_by_key(|e| e.seq);
        Some(events)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Thread-safe wrapper for the event cache.
pub type SharedEventCache = Arc<RwLock<EventCache>>;

/// Spawn a background task that listens to an `EventBroadcast` and feeds
/// new events into the cache.
#[cfg(feature = "broadcast")]
pub fn spawn_cache_listener(
    cache: SharedEventCache,
    broadcast: &crate::broadcast::EventBroadcast,
    display: Arc<dyn crate::display::EventDisplay>,
) {
    let mut rx = broadcast.subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(stored) => {
                    let event = Arc::new(stored.to_inspector_event(display.as_ref()));
                    cache.write().await.push(event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "Event cache listener lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    warn!("Event broadcast channel closed — cache listener stopping");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_event(
        seq: i64,
        event_type: &str,
        payload: &str,
        correlation_id: Option<&str>,
        reactor_id: Option<&str>,
        parent_id: Option<&str>,
    ) -> InspectorEvent {
        InspectorEvent {
            seq,
            ts: Utc::now(),
            event_type: event_type.to_string(),
            name: "test_event".to_string(),
            id: Some(Uuid::new_v4().to_string()),
            parent_id: parent_id.map(String::from),
            correlation_id: correlation_id.map(String::from),
            reactor_id: reactor_id.map(String::from),
            summary: None,
            payload: payload.to_string(),
        }
    }

    #[test]
    fn push_updates_all_indexes() {
        let mut cache = EventCache::new(10);
        let cid = Uuid::new_v4().to_string();
        let event = Arc::new(make_event(1, "TestEvent", r#"{"type":"test"}"#, Some(&cid), Some("reactor-a"), None));

        cache.push(event);

        assert!(cache.by_seq.contains_key(&1));
        assert_eq!(cache.by_handler.get("reactor-a").unwrap(), &vec![1i64]);
        let cid_uuid = Uuid::parse_str(&cid).unwrap();
        assert_eq!(cache.by_correlation.get(&cid_uuid).unwrap(), &vec![1i64]);
    }

    #[test]
    fn cache_evicts_oldest_when_at_capacity() {
        let mut cache = EventCache::new(3);

        for i in 1..=4 {
            let cid = Uuid::new_v4().to_string();
            let event = Arc::new(make_event(i, "TestEvent", "{}", Some(&cid), None, None));
            cache.push(event);
        }

        assert_eq!(cache.len(), 3);
        assert!(cache.by_seq.get(&1).is_none());
        assert!(cache.by_seq.get(&2).is_some());
        assert!(cache.by_seq.get(&4).is_some());
    }

    #[test]
    fn search_matches_payload_text_case_insensitive() {
        let mut cache = EventCache::new(100);
        cache.push(Arc::new(make_event(1, "WorldEvent", r#"{"type":"meeting","title":"Community Meeting"}"#, None, None, None)));
        cache.push(Arc::new(make_event(2, "ScrapeEvent", r#"{"type":"scraped","url":"http://example.com"}"#, None, None, None)));

        let (results, _) = cache.search(Some("community"), None, None, None, None, 50);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].seq, 1);
    }

    #[test]
    fn causal_tree_returns_correlated_events() {
        let mut cache = EventCache::new(100);
        let cid = Uuid::new_v4().to_string();

        cache.push(Arc::new(make_event(1, "TestEvent", "{}", Some(&cid), None, None)));
        cache.push(Arc::new(make_event(2, "TestEvent", "{}", Some(&cid), None, Some("parent-uuid"))));
        cache.push(Arc::new(make_event(3, "TestEvent", "{}", None, None, None)));

        let (tree, root_seq) = cache.causal_tree(1).unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(root_seq, 1);
    }

    #[test]
    fn causal_flow_returns_correlated_events() {
        let mut cache = EventCache::new(100);
        let cid_a = Uuid::new_v4().to_string();
        let cid_b = Uuid::new_v4().to_string();
        cache.push(Arc::new(make_event(1, "TestEvent", "{}", Some(&cid_a), None, None)));
        cache.push(Arc::new(make_event(2, "TestEvent", "{}", Some(&cid_a), None, None)));
        cache.push(Arc::new(make_event(3, "TestEvent", "{}", Some(&cid_b), None, None)));

        let flow = cache.causal_flow(&cid_a).unwrap();
        assert_eq!(flow.len(), 2);
        assert!(cache.causal_flow("00000000-0000-0000-0000-000000000000").is_none());
    }

    #[test]
    fn cursor_pagination() {
        let mut cache = EventCache::new(100);
        for i in 1..=10 {
            cache.push(Arc::new(make_event(i, "TestEvent", "{}", None, None, None)));
        }

        let (page1, cursor1) = cache.search(None, None, None, None, None, 3);
        assert_eq!(page1.len(), 3);
        assert_eq!(page1[0].seq, 10);
        assert_eq!(cursor1, Some(8));

        let (page2, _) = cache.search(None, cursor1, None, None, None, 3);
        assert_eq!(page2[0].seq, 7);
    }
}
