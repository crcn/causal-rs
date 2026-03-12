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
    /// Optional one-line summary from `EventDisplay::summary`.
    pub summary: Option<String>,
    /// JSON payload as string.
    pub payload: String,
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
pub struct ReactorOutcome {
    pub reactor_id: String,
    pub status: String,
    pub error: Option<String>,
    pub attempts: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub triggering_event_ids: Vec<String>,
}
