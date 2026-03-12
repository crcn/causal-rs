use chrono::{DateTime, Utc};

/// Processed event ready for the inspector.
/// Use `EventDisplay` to control `name` and `summary`.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct InspectorEvent {
    pub seq: i64,
    pub ts: DateTime<Utc>,
    /// The event_type column (codec name, e.g. "OrderEvent").
    #[cfg_attr(feature = "graphql", graphql(name = "type"))]
    pub event_type: String,
    /// Human-readable display name from `EventDisplay::display_name`.
    pub name: String,
    /// Causal event UUID.
    pub id: Option<String>,
    /// Parent event UUID (causal link).
    pub parent_id: Option<String>,
    /// Correlation ID linking the full causal chain.
    pub correlation_id: Option<String>,
    pub reactor_id: Option<String>,
    /// Aggregate type (e.g. "Order"), if this event matched an aggregator.
    pub aggregate_type: Option<String>,
    /// Aggregate instance ID, if this event matched an aggregator.
    pub aggregate_id: Option<String>,
    /// Per-stream version, if this event matched an aggregator.
    pub stream_version: Option<i64>,
    /// Optional one-line summary from `EventDisplay::summary`.
    pub summary: Option<String>,
    /// JSON payload as string.
    pub payload: String,
}

impl InspectorEvent {
    /// Composite aggregate key (e.g. "Order:00000000-…"), or `None` if no aggregate identity.
    pub fn aggregate_key(&self) -> Option<String> {
        match (&self.aggregate_type, &self.aggregate_id) {
            (Some(t), Some(id)) => Some(format!("{t}:{id}")),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct InspectorEventsPage {
    pub events: Vec<InspectorEvent>,
    pub next_cursor: Option<i64>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct InspectorCausalTree {
    pub events: Vec<InspectorEvent>,
    pub root_seq: i64,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct InspectorCausalFlow {
    pub events: Vec<InspectorEvent>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct ReactorLog {
    pub event_id: String,
    pub reactor_id: String,
    pub level: String,
    pub message: String,
    pub data: Option<serde_json::Value>,
    pub logged_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct ReactorDescription {
    pub reactor_id: String,
    pub blocks: serde_json::Value,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct ReactorDescriptionSnapshot {
    pub seq: i64,
    pub event_id: String,
    pub reactor_id: String,
    pub blocks: serde_json::Value,
}

/// A single aggregate's state at a point in the correlation timeline.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct AggregateStateEntry {
    /// Aggregate type + ID key (e.g. "pipeline_state:00000000-…").
    pub key: String,
    /// Serialized aggregate state JSON.
    pub state: serde_json::Value,
}

/// Aggregate state snapshot at a specific event in a correlation.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct AggregateTimelineEntry {
    pub seq: i64,
    pub event_id: String,
    pub event_type: String,
    /// All aggregate states after this event was applied.
    pub aggregates: Vec<AggregateStateEntry>,
}

/// A reactor's input/output dependency edges for the dependency map.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct ReactorDependency {
    pub reactor_id: String,
    pub input_event_types: Vec<String>,
    pub output_event_types: Vec<String>,
}

/// An aggregate lifecycle entry — state snapshot across all correlations.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct AggregateLifecycleEntry {
    pub seq: i64,
    pub event_id: String,
    pub event_type: String,
    pub ts: DateTime<Utc>,
    pub correlation_id: String,
    pub aggregate_key: String,
    pub state: serde_json::Value,
}

/// Summary of a correlation chain for the explorer pane.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct CorrelationSummary {
    pub correlation_id: String,
    pub event_count: i64,
    pub first_ts: DateTime<Utc>,
    pub last_ts: DateTime<Utc>,
    pub root_event_type: String,
    pub has_errors: bool,
}

/// Paginated correlation summary response.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct CorrelationSummaryPage {
    pub correlations: Vec<CorrelationSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct ReactorOutcome {
    pub reactor_id: String,
    pub status: String,
    pub error: Option<String>,
    pub attempts: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub triggering_event_ids: Vec<String>,
}
