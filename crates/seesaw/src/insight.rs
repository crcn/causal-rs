//! Insight types for observability callbacks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Event for insight visualization stream
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightEvent {
    /// Sequence number (for cursor-based pagination)
    pub seq: i64,
    /// Stream entry type
    pub stream_type: StreamType,
    /// Correlation ID (workflow identifier)
    pub correlation_id: Uuid,
    /// Event ID (if event-related)
    pub event_id: Option<Uuid>,
    /// Effect event ID (if effect-related)
    pub effect_event_id: Option<Uuid>,
    /// Effect ID (if effect-related)
    pub handler_id: Option<String>,
    /// Event type name
    pub event_type: Option<String>,
    /// Status (for effect lifecycle)
    pub status: Option<String>,
    /// Error message (if failed)
    pub error: Option<String>,
    /// Payload (JSON)
    pub payload: Option<serde_json::Value>,
    /// When this entry was created
    pub created_at: DateTime<Utc>,
}

/// Type of stream entry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    /// Event dispatched
    EventDispatched,
    /// Effect started
    EffectStarted,
    /// Effect completed
    EffectCompleted,
    /// Effect failed
    EffectFailed,
}
