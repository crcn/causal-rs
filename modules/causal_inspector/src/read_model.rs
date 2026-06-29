//! Store-agnostic inspector read model trait.
//!
//! Defines the query interface that the inspector needs to read events
//! and reactor metadata. Implement this trait for your store backend.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::display::EventDisplay;
use crate::types::InspectorEvent;

/// A stored event before display transformation.
///
/// This is the common type returned by all `InspectorReadModel` implementations.
/// The inspector layer applies [`EventDisplay`] to convert it into an [`InspectorEvent`].
#[derive(Debug, Clone)]
pub struct StoredEvent {
    /// Global sequence number (monotonically increasing).
    pub seq: i64,
    /// When the event was persisted.
    pub ts: DateTime<Utc>,
    /// Stable event type name (e.g. "OrderPlaced").
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// Unique event ID.
    pub id: Option<Uuid>,
    /// Parent event ID (causal link).
    pub causation_id: Option<Uuid>,
    /// Workflow ID linking the full causal chain.
    pub workflow_id: Option<Uuid>,
    /// Reactor that produced this event.
    pub reactor_id: Option<String>,
    /// Aggregate type (e.g. "Order"), if this event matched an aggregator.
    pub aggregate_type: Option<String>,
    /// Aggregate instance ID, if this event matched an aggregator.
    pub aggregate_id: Option<Uuid>,
    /// Per-stream version, if this event matched an aggregator.
    pub stream_revision: Option<u64>,
}

impl StoredEvent {
    /// Convert to an [`InspectorEvent`] using the given display.
    pub fn to_inspector_event(&self, display: &dyn EventDisplay) -> InspectorEvent {
        let name = display.display_name(&self.event_type, &self.payload);
        let summary = display.summary(&self.event_type, &self.payload);
        let payload_str = serde_json::to_string(&self.payload).unwrap_or_default();

        InspectorEvent {
            seq: self.seq,
            ts: self.ts,
            event_type: self.event_type.clone(),
            name,
            id: self.id.map(|u| u.to_string()),
            causation_id: self.causation_id.map(|u| u.to_string()),
            workflow_id: self.workflow_id.map(|u| u.to_string()),
            reactor_id: self.reactor_id.clone(),
            aggregate_type: self.aggregate_type.clone(),
            aggregate_id: self.aggregate_id.map(|u| u.to_string()),
            stream_revision: self.stream_revision.map(|v| v as i64),
            summary,
            payload: payload_str,
        }
    }

    /// Composite aggregate key (e.g. "Order:00000000-…"), or `None` if no aggregate identity.
    pub fn aggregate_key(&self) -> Option<String> {
        match (&self.aggregate_type, &self.aggregate_id) {
            (Some(t), Some(id)) => Some(format!("{t}:{id}")),
            _ => None,
        }
    }
}

/// Filters for paginated event listing.
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// Max events to return.
    pub limit: usize,
    /// Cursor for pagination (seq of last seen event).
    pub cursor: Option<i64>,
    /// Full-text search across payload, event_type, workflow_id.
    pub search: Option<String>,
    /// Only events after this time.
    pub from: Option<DateTime<Utc>>,
    /// Only events before this time.
    pub to: Option<DateTime<Utc>>,
    /// Filter to a specific workflow chain.
    pub workflow_id: Option<String>,
    /// Filter to a specific aggregate stream (e.g. "Order:00000000-…").
    pub aggregate_key: Option<String>,
}

/// A reactor log entry.
#[derive(Debug, Clone)]
pub struct ReactorLogEntry {
    pub event_id: Uuid,
    pub reactor_id: String,
    pub level: String,
    pub message: String,
    pub data: Option<serde_json::Value>,
    pub logged_at: DateTime<Utc>,
}

/// Aggregated reactor execution outcome.
#[derive(Debug, Clone)]
pub struct ReactorOutcomeEntry {
    pub reactor_id: String,
    pub status: String,
    pub error: Option<String>,
    pub attempts: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub triggering_event_ids: Vec<String>,
    /// True when this reactor accepted a divergent redelivery for the
    /// workflow — a nondeterministic output whose canonical row was kept.
    /// Orthogonal to `status` (which stays `completed`): the reaction ran,
    /// but its re-emission diverged. Never an error/dead_letter.
    pub diverged: bool,
}

/// Per-attempt execution record for a reactor.
#[derive(Debug, Clone)]
pub struct ReactorAttemptEntry {
    pub event_id: Uuid,
    pub reactor_id: String,
    pub workflow_id: String,
    pub attempt: i32,
    pub status: String,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

/// Reactor description blocks for the inspector.
#[derive(Debug, Clone)]
pub struct ReactorDescriptionEntry {
    pub reactor_id: String,
    pub description: serde_json::Value,
}

/// A point-in-time snapshot of a reactor description at a specific event.
#[derive(Debug, Clone)]
pub struct ReactorDescriptionSnapshotEntry {
    pub seq: i64,
    pub event_id: Uuid,
    pub reactor_id: String,
    pub description: serde_json::Value,
}

/// A point-in-time snapshot of aggregate state at a specific event.
#[derive(Debug, Clone)]
pub struct AggregateStateSnapshotEntry {
    pub seq: i64,
    pub event_id: Uuid,
    pub event_type: String,
    pub aggregate_key: String,
    pub state: serde_json::Value,
}

/// A reactor's input/output dependency edges.
#[derive(Debug, Clone)]
pub struct ReactorDependencyEntry {
    pub reactor_id: String,
    /// Event types that trigger this reactor (inputs).
    pub input_event_types: Vec<String>,
    /// Event types this reactor produces (outputs).
    pub output_event_types: Vec<String>,
}

/// An aggregate lifecycle entry — state snapshot across all workflows.
#[derive(Debug, Clone)]
pub struct AggregateLifecycleEntry {
    pub seq: i64,
    pub event_id: Uuid,
    pub event_type: String,
    pub ts: DateTime<Utc>,
    pub workflow_id: String,
    pub aggregate_key: String,
    pub state: serde_json::Value,
}

/// Summary of a workflow chain for the explorer pane.
#[derive(Debug, Clone)]
pub struct CorrelationSummaryEntry {
    pub workflow_id: String,
    pub event_count: i64,
    pub first_ts: DateTime<Utc>,
    pub last_ts: DateTime<Utc>,
    pub root_event_type: String,
    pub has_errors: bool,
}

/// One `ctx.effect()` result associated with a triggering event.
#[derive(Debug, Clone)]
pub struct EffectRecord {
    pub consumer:   String,
    pub label:      String,
    pub value:      serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Whether a subject-chain event came from the entity's own stream or from a
/// downstream descendant triggered by that stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "graphql", derive(async_graphql::Enum))]
pub enum SubjectChainSourceMode {
    Stream,
    Descendant,
}

/// Query mode for `subject_chain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "graphql", derive(async_graphql::Enum))]
pub enum SubjectChainMode {
    /// Only events whose aggregate_type/aggregate_id matches the subject.
    Stream,
    /// Only events caused (directly or transitively) by the subject's stream events.
    Descendants,
    /// Both stream and descendant events merged by position, stream wins on conflict.
    Both,
}

/// A raw event from a subject chain query, before display transformation.
#[derive(Debug, Clone)]
pub struct SubjectChainEventRaw {
    pub stored:      StoredEvent,
    pub source_mode: SubjectChainSourceMode,
}

/// Raw paginated result from `subject_chain`, before display transformation.
#[derive(Debug, Clone)]
pub struct SubjectChainPage {
    pub events:            Vec<SubjectChainEventRaw>,
    /// Position of the last returned event, for the next page cursor.
    pub next_cursor:       Option<i64>,
    /// True if the descendant tree was truncated at the depth cap (depth 10).
    pub depth_cap_reached: bool,
}

/// One entity (aggregate) entry with its first-event data for label extraction.
#[derive(Debug, Clone)]
pub struct AggregateKeyEntry {
    pub aggregate_id:   Uuid,
    pub event_type:     String,
    pub first_payload:  serde_json::Value,
}

/// Paginated entity list from `list_aggregate_keys_by_type`.
#[derive(Debug, Clone)]
pub struct AggregateKeysPage {
    pub entries:     Vec<AggregateKeyEntry>,
    /// aggregate_id of the last entry, for the next page cursor.
    pub next_cursor: Option<Uuid>,
}

/// Store-agnostic read model for the inspector.
///
/// Implement this trait for your store backend (Postgres, in-memory, etc.)
/// and inject it into the GraphQL context as `Arc<dyn InspectorReadModel>`.
///
/// # Example
///
/// ```ignore
/// let store = Arc::new(MemoryStore::new());
/// schema_builder.data(Arc::new(MemoryInspectorReadModel::new(store)) as Arc<dyn InspectorReadModel>);
/// ```
#[async_trait]
pub trait InspectorReadModel: Send + Sync {
    /// Paginated reverse-chronological event listing with optional filters.
    async fn list_events(&self, query: &EventQuery) -> Result<Vec<StoredEvent>>;

    /// Single event by sequence number.
    async fn get_event(&self, seq: i64) -> Result<Option<StoredEvent>>;

    /// All events sharing the same workflow_id as the given event.
    ///
    /// Returns `(events, root_seq)` where `root_seq` is the seq of the
    /// root event (the one with no parent).
    async fn causal_tree(&self, seq: i64) -> Result<(Vec<StoredEvent>, i64)>;

    /// All events sharing a workflow_id, ordered by seq ascending.
    async fn causal_flow(&self, workflow_id: &str) -> Result<Vec<StoredEvent>>;

    /// Events starting from a given seq (for subscription catch-up).
    async fn events_from_seq(&self, start_seq: i64, limit: usize) -> Result<Vec<StoredEvent>>;

    // ── Reactor observability ────────────────────────────────────

    /// Logs for a specific event + reactor.
    async fn reactor_logs(
        &self,
        event_id: Uuid,
        reactor_id: &str,
    ) -> Result<Vec<ReactorLogEntry>>;

    /// All reactor logs for a workflow chain.
    async fn reactor_logs_by_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorLogEntry>>;

    /// Aggregated reactor execution outcomes for a workflow chain.
    async fn reactor_outcomes(&self, workflow_id: &str) -> Result<Vec<ReactorOutcomeEntry>>;

    /// Per-attempt execution history for a workflow chain.
    ///
    /// Returns individual attempt records ordered by started_at ascending.
    async fn reactor_attempt_history(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorAttemptEntry>>;

    /// Reactor description blocks for a workflow chain.
    async fn reactor_descriptions(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorDescriptionEntry>>;

    /// Per-event description snapshots for a workflow chain.
    ///
    /// Returns snapshots ordered by seq ascending, showing the state of
    /// each reactor's description at each event in the workflow.
    async fn reactor_description_snapshots(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ReactorDescriptionSnapshotEntry>>;

    /// Per-event aggregate state snapshots for a workflow chain.
    ///
    /// Returns snapshots ordered by seq ascending, showing aggregate state
    /// after each event was applied.
    async fn aggregate_state_timeline(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<AggregateStateSnapshotEntry>>;

    /// List all workflow chains with summary stats.
    ///
    /// Optionally filter by search string (matches workflow_id or root event type).
    /// Returns most recent workflows first, limited by `limit`.
    /// Pass `cursor` (last_ts of previous page's last item) for pagination.
    async fn list_workflows(
        &self,
        search: Option<&str>,
        limit: usize,
        cursor: Option<DateTime<Utc>>,
    ) -> Result<Vec<CorrelationSummaryEntry>>;

    /// Derive the reactor dependency graph from the event log.
    ///
    /// For each reactor, returns the event types it was triggered by (inputs)
    /// and the event types it produced (outputs).
    async fn reactor_dependencies(&self) -> Result<Vec<ReactorDependencyEntry>>;

    /// All state snapshots for a specific aggregate across all workflows.
    ///
    /// Returns entries ordered by seq ascending.
    async fn aggregate_lifecycle(
        &self,
        aggregate_key: &str,
        limit: usize,
    ) -> Result<Vec<AggregateLifecycleEntry>>;

    /// List all known aggregate keys.
    async fn list_aggregate_keys(&self) -> Result<Vec<String>>;

    // ── Entity-scoped inspection ─────────────────────────────────

    /// All `ctx.effect()` results triggered by the given event.
    async fn effects_for_event(&self, event_id: Uuid) -> Result<Vec<EffectRecord>>;

    /// Distinct aggregate types present in the event log, optionally filtered.
    async fn list_aggregate_types(
        &self,
        search: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>>;

    /// Entities of a given type with the first event's data for label extraction.
    /// Cursor is the `aggregate_id` of the last seen entry.
    async fn list_aggregate_keys_by_type(
        &self,
        aggregate_type: &str,
        search: Option<&str>,
        limit: usize,
        cursor: Option<Uuid>,
    ) -> Result<AggregateKeysPage>;

    /// Events for an entity scoped by mode, paginated by position cursor.
    async fn subject_chain(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        mode: SubjectChainMode,
        limit: usize,
        cursor: Option<i64>,
    ) -> Result<SubjectChainPage>;
}
