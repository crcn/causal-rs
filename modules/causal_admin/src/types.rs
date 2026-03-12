use chrono::{DateTime, Utc};

/// Processed event ready for the admin UI.
/// Use `EventDisplay` to control `name`, `layer`, and `summary`.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct AdminEvent {
    pub seq: i64,
    pub ts: DateTime<Utc>,
    /// The event_type column (codec name, e.g. "OrderEvent").
    #[cfg_attr(feature = "graphql", graphql(name = "type"))]
    pub event_type: String,
    /// Human-readable display name from `EventDisplay::display_name`.
    pub name: String,
    /// Layer classification from `EventDisplay::layer`.
    pub layer: String,
    /// Causal event UUID.
    pub id: Option<String>,
    /// Parent event UUID (causal link).
    pub parent_id: Option<String>,
    pub correlation_id: Option<String>,
    pub run_id: Option<String>,
    pub reactor_id: Option<String>,
    /// Optional one-line summary from `EventDisplay::summary`.
    pub summary: Option<String>,
    /// JSON payload as string.
    pub payload: String,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct AdminEventsPage {
    pub events: Vec<AdminEvent>,
    pub next_cursor: Option<i64>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct AdminCausalTree {
    pub events: Vec<AdminEvent>,
    pub root_seq: i64,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct AdminCausalFlow {
    pub events: Vec<AdminEvent>,
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
