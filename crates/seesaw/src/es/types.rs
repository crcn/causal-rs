use std::ops::{Deref, DerefMut};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::upcast::EventUpcast;

/// A persisted event loaded from the event store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: Uuid,
    /// Global ordering (for tailing the log).
    pub position: u64,
    pub aggregate_id: Uuid,
    /// Per-aggregate ordering (1-based).
    pub sequence: u64,
    pub event_type: String,
    pub data: serde_json::Value,
    pub metadata: serde_json::Value,
    pub schema_version: u32,
    /// ID of the causing event (cross-aggregate causal chain).
    pub caused_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// An event to be appended to the event store.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub event_type: String,
    pub data: serde_json::Value,
    pub metadata: Option<serde_json::Value>,
    pub schema_version: u32,
    pub caused_by: Option<Uuid>,
}

impl NewEvent {
    /// Create a `NewEvent` from any serializable event that implements `EventUpcast`.
    pub fn from_event<E: Serialize + EventUpcast>(event: &E) -> anyhow::Result<Self> {
        Ok(Self {
            event_type: std::any::type_name::<E>().to_string(),
            data: serde_json::to_value(event)?,
            metadata: None,
            schema_version: E::current_version(),
            caused_by: None,
        })
    }

    /// Set the causing event ID.
    pub fn caused_by(mut self, id: Uuid) -> Self {
        self.caused_by = Some(id);
        self
    }

    /// Attach metadata.
    pub fn with_metadata(mut self, meta: serde_json::Value) -> Self {
        self.metadata = Some(meta);
        self
    }
}

/// Concurrency conflict error.
#[derive(Debug, thiserror::Error)]
#[error("concurrency conflict on {aggregate_id}: expected version {expected}, actual {actual}")]
pub struct ConcurrencyError {
    pub aggregate_id: Uuid,
    pub expected: u64,
    pub actual: u64,
}

/// Wraps aggregate state with its version. Aggregates don't track version themselves.
#[derive(Debug, Clone)]
pub struct Versioned<A> {
    pub state: A,
    pub version: u64,
}

impl<A> Versioned<A> {
    pub fn new(state: A, version: u64) -> Self {
        Self { state, version }
    }

    pub fn version(&self) -> u64 {
        self.version
    }
}

impl<A> Deref for Versioned<A> {
    type Target = A;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl<A> DerefMut for Versioned<A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}
