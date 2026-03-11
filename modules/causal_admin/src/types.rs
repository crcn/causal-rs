use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Raw event row from the `events` table.
pub struct EventRow {
    pub id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub seq: i64,
    pub ts: DateTime<Utc>,
    pub event_type: String,
    pub data: serde_json::Value,
    pub run_id: Option<String>,
    pub correlation_id: Option<Uuid>,
    pub parent_seq: Option<i64>,
    pub handler_id: Option<String>,
}

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
    pub handler_id: Option<String>,
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
pub struct HandlerLog {
    pub event_id: String,
    pub handler_id: String,
    pub level: String,
    pub message: String,
    pub data: Option<serde_json::Value>,
    pub logged_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct HandlerDescription {
    pub handler_id: String,
    pub blocks: serde_json::Value,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "graphql", derive(async_graphql::SimpleObject))]
pub struct HandlerOutcome {
    pub handler_id: String,
    pub status: String,
    pub error: Option<String>,
    pub attempts: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub triggering_event_ids: Vec<String>,
}

impl AdminEvent {
    /// Convert an `EventRow` into an `AdminEvent` using the given display.
    pub fn from_row(row: EventRow, display: &dyn crate::display::EventDisplay) -> Self {
        let name = display.display_name(&row.event_type, &row.data);
        let summary = display.summary(&row.event_type, &row.data);
        let layer = display.layer(&row.event_type).to_string();
        let payload = serde_json::to_string(&row.data).unwrap_or_default();

        Self {
            seq: row.seq,
            ts: row.ts,
            event_type: row.event_type,
            name,
            layer,
            id: row.id.map(|u| u.to_string()),
            parent_id: row.parent_id.map(|u| u.to_string()),
            correlation_id: row.correlation_id.map(|u| u.to_string()),
            run_id: row.run_id,
            handler_id: row.handler_id,
            summary,
            payload,
        }
    }
}
