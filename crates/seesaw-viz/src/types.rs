//! Core types for event span tracking and visualization

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::any::TypeId;
use std::collections::HashMap;
use uuid::Uuid;

/// Represents a single event execution span
#[derive(Debug, Clone, Serialize)]
pub struct EventSpan {
    /// Unique span ID
    pub span_id: Uuid,

    /// Event ID that triggered this span
    pub event_id: Uuid,

    /// Parent event ID (for causality tracking)
    pub parent_event_id: Option<Uuid>,

    /// Event type name
    pub event_type: String,

    /// Event type ID (for filtering)
    #[serde(skip)]
    pub event_type_id: TypeId,

    /// Effect name that processed this event
    pub effect_name: Option<String>,

    /// Module path of the effect (for component grouping)
    pub module_path: Option<String>,

    /// When this span started
    pub started_at: DateTime<Utc>,

    /// When this span completed (None if still running)
    pub completed_at: Option<DateTime<Utc>>,

    /// Duration in microseconds
    pub duration_us: Option<u64>,

    /// State before effect execution (serialized)
    pub prev_state: Option<serde_json::Value>,

    /// State after effect execution (serialized)
    pub next_state: Option<serde_json::Value>,

    /// State diff (computed lazily)
    pub state_diff: Option<serde_json::Value>,

    /// Whether this effect succeeded
    pub success: bool,

    /// Error message if failed
    pub error: Option<String>,

    /// Custom attributes (for user-defined metadata)
    pub attributes: HashMap<String, String>,
}

impl EventSpan {
    /// Calculate duration if completed
    pub fn duration(&self) -> Option<chrono::Duration> {
        self.completed_at.map(|end| end - self.started_at)
    }

    /// Check if this span has completed
    pub fn is_complete(&self) -> bool {
        self.completed_at.is_some()
    }

    /// Get component name from module path
    pub fn component(&self) -> String {
        self.module_path
            .as_ref()
            .and_then(|path| path.split("::").next())
            .unwrap_or("unknown")
            .to_string()
    }
}

/// A graph of event spans with causal relationships
#[derive(Debug, Default, Serialize, Clone)]
pub struct SpanGraph {
    /// All spans indexed by span_id
    pub spans: HashMap<Uuid, EventSpan>,

    /// Parent-child relationships (parent_event_id -> [child_event_ids])
    pub children: HashMap<Uuid, Vec<Uuid>>,

    /// Root spans (no parent)
    pub roots: Vec<Uuid>,
}

impl SpanGraph {
    /// Add a span to the graph
    pub fn add_span(&mut self, span: EventSpan) {
        let span_id = span.span_id;
        let parent_id = span.parent_event_id;

        // Add to children map
        if let Some(parent) = parent_id {
            self.children.entry(parent).or_default().push(span.event_id);
        } else {
            // Root span (no parent)
            self.roots.push(span.event_id);
        }

        self.spans.insert(span_id, span);
    }

    /// Get all children of a given event
    pub fn get_children(&self, event_id: Uuid) -> Vec<&EventSpan> {
        self.children
            .get(&event_id)
            .map(|child_ids| {
                child_ids
                    .iter()
                    .filter_map(|id| self.find_span_by_event_id(*id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Find span by event_id
    pub fn find_span_by_event_id(&self, event_id: Uuid) -> Option<&EventSpan> {
        self.spans.values().find(|s| s.event_id == event_id)
    }

    /// Get all root spans (entry points)
    pub fn root_spans(&self) -> Vec<&EventSpan> {
        self.roots
            .iter()
            .filter_map(|id| self.find_span_by_event_id(*id))
            .collect()
    }

    /// Build causal chains from roots
    pub fn causal_chains(&self) -> Vec<CausalChain> {
        self.root_spans()
            .into_iter()
            .map(|root| self.build_chain(root))
            .collect()
    }

    fn build_chain(&self, span: &EventSpan) -> CausalChain {
        let children = self
            .get_children(span.event_id)
            .into_iter()
            .map(|child| self.build_chain(child))
            .collect();

        CausalChain {
            span: span.clone(),
            children,
        }
    }
}

/// A causal chain of events (tree structure)
#[derive(Debug, Clone)]
pub struct CausalChain {
    /// The event span
    pub span: EventSpan,

    /// Child chains
    pub children: Vec<CausalChain>,
}

impl CausalChain {
    /// Traverse the chain depth-first
    pub fn walk<F>(&self, mut f: F)
    where
        F: FnMut(&EventSpan),
    {
        f(&self.span);
        for child in &self.children {
            child.walk(&mut f);
        }
    }

    /// Calculate total duration (including children)
    pub fn total_duration(&self) -> Option<chrono::Duration> {
        let mut total = self.span.duration()?;
        for child in &self.children {
            if let Some(child_duration) = child.total_duration() {
                total = total + child_duration;
            }
        }
        Some(total)
    }

    /// Count total events in chain
    pub fn event_count(&self) -> usize {
        1 + self.children.iter().map(|c| c.event_count()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_span(event_id: Uuid, parent_id: Option<Uuid>) -> EventSpan {
        EventSpan {
            span_id: Uuid::new_v4(),
            event_id,
            parent_event_id: parent_id,
            event_type: "TestEvent".into(),
            event_type_id: TypeId::of::<()>(),
            effect_name: Some("test_effect".into()),
            module_path: Some("test::module".into()),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            duration_us: Some(1000),
            prev_state: None,
            next_state: None,
            state_diff: None,
            success: true,
            error: None,
            attributes: HashMap::new(),
        }
    }

    #[test]
    fn test_span_graph_root() {
        let mut graph = SpanGraph::default();
        let root_id = Uuid::new_v4();

        graph.add_span(make_span(root_id, None));

        assert_eq!(graph.roots.len(), 1);
        assert_eq!(graph.root_spans().len(), 1);
    }

    #[test]
    fn test_span_graph_children() {
        let mut graph = SpanGraph::default();
        let root_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();

        graph.add_span(make_span(root_id, None));
        graph.add_span(make_span(child_id, Some(root_id)));

        let children = graph.get_children(root_id);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].event_id, child_id);
    }

    #[test]
    fn test_causal_chain() {
        let mut graph = SpanGraph::default();
        let root_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();

        graph.add_span(make_span(root_id, None));
        graph.add_span(make_span(child_id, Some(root_id)));

        let chains = graph.causal_chains();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].event_count(), 2);
    }
}
