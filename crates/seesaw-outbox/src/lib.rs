//! Durable event outbox for same-transaction event persistence.
//!
//! The outbox pattern ensures events are persisted in the same database transaction
//! as business data, providing durability guarantees without distributed transactions.
//!
//! # Overview
//!
//! 1. Effect writes business data AND outbox entry in single transaction
//! 2. Background publisher polls outbox, emits to EventBus, marks as published
//! 3. Cleanup job removes old published entries
//!
//! # Guarantees
//!
//! - **At-least-once delivery**: Events may be re-delivered after publisher crash
//! - **Same-transaction durability**: Event survives if business write survives
//! - **Multi-instance safe**: Uses `FOR UPDATE SKIP LOCKED` for concurrent publishers
//!
//! # Schema Evolution
//!
//! Events are persisted as JSONB with versioned event types:
//! - `event_type()` includes version: `"notification.created.v1"`
//! - New versions = new type: `NotificationCreatedV2` with `"notification.created.v2"`
//! - Registry maps both: Old events still deserialize, new code emits new version
//! - No in-place migration: Old rows stay as-is
//!
//! # Example
//!
//! ```ignore
//! use seesaw_outbox::{CorrelationId, OutboxEvent, OutboxWriter};
//! use seesaw_core::EffectContext;
//!
//! // 1. Mark event for outbox persistence
//! #[derive(Debug, Clone, Serialize, Deserialize)]
//! pub struct NotificationCreated {
//!     pub id: Uuid,
//!     pub user_id: Uuid,
//! }
//!
//! impl OutboxEvent for NotificationCreated {
//!     fn event_type() -> &'static str { "notification.created.v1" }
//! }
//!
//! // 2. In effect, write to outbox in same transaction
//! async fn create_notification(
//!     cmd: CreateNotificationCmd,
//!     ctx: &EffectContext<State, Deps>,
//! ) -> Result<()> {
//!     let mut tx = ctx.deps().db.begin().await?;
//!
//!     // Business write
//!     let notification = Notification::create(&cmd, &mut tx).await?;
//!
//!     // Outbox write (same transaction)
//!     let mut writer = PgOutboxWriter::new(&mut tx);
//!     writer.write_event(
//!         &NotificationCreated { id: notification.id, user_id: cmd.user_id },
//!         CorrelationId::new(), // or track correlation across operations
//!     ).await?;
//!
//!     tx.commit().await?;
//!     Ok(())
//! }
//! ```

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use uuid::Uuid;

// CorrelationId type for outbox tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CorrelationId(Option<Uuid>);

impl CorrelationId {
    /// No correlation ID.
    pub const NONE: Self = Self(None);

    /// Create a new random correlation ID.
    pub fn new() -> Self {
        Self(Some(Uuid::new_v4()))
    }

    /// Check if this is NONE.
    pub fn is_none(&self) -> bool {
        self.0.is_none()
    }

    /// Check if this has a value.
    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }

    /// Get the inner UUID (returns nil UUID if NONE).
    pub fn into_inner(self) -> Uuid {
        self.0.unwrap_or_else(Uuid::nil)
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Uuid> for CorrelationId {
    fn from(uuid: Uuid) -> Self {
        Self(Some(uuid))
    }
}

impl From<Option<Uuid>> for CorrelationId {
    fn from(opt: Option<Uuid>) -> Self {
        Self(opt)
    }
}

impl std::fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(uuid) => write!(f, "{}", uuid),
            None => write!(f, "NONE"),
        }
    }
}

// =============================================================================
// OutboxEvent Trait
// =============================================================================

/// An event that can be persisted to the transactional outbox.
///
/// Events implementing this trait can be written to the outbox within a database
/// transaction, ensuring durability alongside business data.
///
/// # Event Type Versioning
///
/// The `event_type()` method should return a versioned string identifier:
/// - Format: `"domain.event.vN"` (e.g., `"notification.created.v1"`)
/// - New versions are new types (e.g., `NotificationCreatedV2`)
/// - Registry can map multiple versions for backwards compatibility
///
/// # Example
///
/// ```ignore
/// use seesaw_outbox::OutboxEvent;
/// use serde::{Deserialize, Serialize};
/// use uuid::Uuid;
///
/// #[derive(Debug, Clone, Serialize, Deserialize)]
/// pub struct OrderShipped {
///     pub order_id: Uuid,
///     pub tracking_number: String,
/// }
///
/// impl OutboxEvent for OrderShipped {
///     fn event_type() -> &'static str {
///         "order.shipped.v1"
///     }
/// }
/// ```
pub trait OutboxEvent: Send + Sync + Clone + Serialize + DeserializeOwned + 'static {
    /// Returns the versioned event type identifier.
    ///
    /// This is used for:
    /// - Routing events to the correct deserializer
    /// - Schema evolution (new versions = new identifiers)
    /// - Filtering/querying the outbox table
    fn event_type() -> &'static str;
}

// =============================================================================
// OutboxEntry
// =============================================================================

/// A persisted outbox entry ready for publishing.
///
/// This struct represents a row from the `event_outbox` table. The publisher
/// reads unpublished entries, deserializes them, emits to the EventBus, and
/// marks them as published.
#[derive(Debug, Clone)]
pub struct OutboxEntry {
    /// Unique identifier for this outbox entry.
    pub id: Uuid,
    /// Versioned event type (e.g., `"notification.created.v1"`).
    pub event_type: String,
    /// Serialized event payload as JSON.
    pub payload: serde_json::Value,
    /// Correlation ID for tracking related work.
    pub correlation_id: CorrelationId,
    /// When the entry was created.
    pub created_at: DateTime<Utc>,
    /// When the entry was published (None if unpublished).
    pub published_at: Option<DateTime<Utc>>,
}

// =============================================================================
// OutboxWriter Trait
// =============================================================================

/// Writes events to the transactional outbox.
///
/// Implementations should write within the same database transaction as
/// business data to ensure atomicity.
///
/// # Example Implementation (PostgreSQL)
///
/// ```ignore
/// pub struct PgOutboxWriter<'a> {
///     tx: &'a mut sqlx::Transaction<'static, sqlx::Postgres>,
/// }
///
/// #[async_trait]
/// impl OutboxWriter for PgOutboxWriter<'_> {
///     async fn write_event<E: OutboxEvent>(
///         &mut self,
///         event: &E,
///         correlation_id: CorrelationId,
///     ) -> Result<Uuid> {
///         let id = Uuid::new_v4();
///         let payload = serde_json::to_value(event)?;
///         let cid: Option<Uuid> = if correlation_id.is_none() {
///             None
///         } else {
///             Some(correlation_id.into())
///         };
///
///         sqlx::query!(
///             r#"INSERT INTO event_outbox (id, event_type, payload, correlation_id)
///                VALUES ($1, $2, $3, $4)"#,
///             id,
///             E::event_type(),
///             payload,
///             cid,
///         )
///         .execute(&mut **self.tx)
///         .await?;
///
///         Ok(id)
///     }
/// }
/// ```
#[async_trait]
pub trait OutboxWriter: Send + Sync {
    /// Write an event to the outbox.
    ///
    /// Returns the generated outbox entry ID.
    async fn write_event<E: OutboxEvent + Send + Sync>(
        &mut self,
        event: &E,
        correlation_id: CorrelationId,
    ) -> Result<Uuid>;
}

// =============================================================================
// OutboxReader Trait
// =============================================================================

/// Reads and manages outbox entries for the publisher.
///
/// Implementations should use database-level locking to ensure multi-instance
/// safety (e.g., `FOR UPDATE SKIP LOCKED` in PostgreSQL).
///
/// # Example Implementation (PostgreSQL)
///
/// ```ignore
/// pub struct PgOutboxReader {
///     pool: sqlx::PgPool,
/// }
///
/// #[async_trait]
/// impl OutboxReader for PgOutboxReader {
///     async fn claim_unpublished(&self, limit: usize) -> Result<Vec<OutboxEntry>> {
///         sqlx::query_as!(
///             OutboxEntry,
///             r#"SELECT id, event_type, payload, correlation_id, created_at, published_at
///                FROM event_outbox
///                WHERE published_at IS NULL
///                ORDER BY created_at ASC
///                LIMIT $1
///                FOR UPDATE SKIP LOCKED"#,
///             limit as i64,
///         )
///         .fetch_all(&self.pool)
///         .await
///         .map_err(Into::into)
///     }
///
///     async fn mark_published(&self, ids: &[Uuid]) -> Result<()> {
///         sqlx::query!(
///             "UPDATE event_outbox SET published_at = NOW() WHERE id = ANY($1)",
///             ids,
///         )
///         .execute(&self.pool)
///         .await?;
///         Ok(())
///     }
///
///     async fn cleanup_published(&self, older_than: DateTime<Utc>) -> Result<u64> {
///         let result = sqlx::query!(
///             "DELETE FROM event_outbox WHERE published_at IS NOT NULL AND published_at < $1",
///             older_than,
///         )
///         .execute(&self.pool)
///         .await?;
///         Ok(result.rows_affected())
///     }
/// }
/// ```
#[async_trait]
pub trait OutboxReader: Send + Sync {
    /// Claim unpublished entries for processing.
    ///
    /// Should use `FOR UPDATE SKIP LOCKED` or equivalent for multi-instance safety.
    /// Returns entries in creation order (oldest first).
    async fn claim_unpublished(&self, limit: usize) -> Result<Vec<OutboxEntry>>;

    /// Mark entries as published.
    ///
    /// Called after successfully emitting to the EventBus.
    async fn mark_published(&self, ids: &[Uuid]) -> Result<()>;

    /// Delete old published entries.
    ///
    /// Returns the number of entries deleted.
    async fn cleanup_published(&self, older_than: DateTime<Utc>) -> Result<u64>;
}

// =============================================================================
// OutboxEventRegistry Trait
// =============================================================================

/// A callback function for emitting events.
///
/// The publisher will provide an emit callback that accepts type-erased events.
/// The registry deserializes entries and calls this callback.
pub type EmitCallback = Arc<dyn Fn(Arc<dyn std::any::Any + Send + Sync>, std::any::TypeId) + Send + Sync>;

/// Registry for deserializing and emitting outbox events.
///
/// Maps event type strings to deserialize+emit functions. Used by the
/// publisher to convert outbox entries back into typed events.
///
/// # Example Implementation
///
/// ```ignore
/// use std::any::{Any, TypeId};
/// use std::collections::HashMap;
/// use std::sync::Arc;
/// use anyhow::{anyhow, Result};
/// use seesaw_outbox::{OutboxEvent, OutboxEntry, OutboxEventRegistry, EmitCallback};
///
/// pub struct DurableEventRegistry {
///     handlers: HashMap<
///         &'static str,
///         Box<dyn Fn(&OutboxEntry, &EmitCallback) -> Result<()> + Send + Sync>
///     >,
/// }
///
/// impl DurableEventRegistry {
///     pub fn new() -> Self {
///         Self { handlers: HashMap::new() }
///     }
///
///     pub fn register<E: OutboxEvent>(mut self) -> Self {
///         self.handlers.insert(E::event_type(), Box::new(|entry, emit| {
///             let event: E = serde_json::from_value(entry.payload.clone())?;
///             let type_id = TypeId::of::<E>();
///             let event_arc: Arc<dyn Any + Send + Sync> = Arc::new(event);
///             emit(event_arc, type_id);
///             Ok(())
///         }));
///         self
///     }
/// }
///
/// impl OutboxEventRegistry for DurableEventRegistry {
///     fn emit_entry(&self, entry: &OutboxEntry, emit: &EmitCallback) -> Result<()> {
///         let handler = self.handlers.get(entry.event_type.as_str())
///             .ok_or_else(|| anyhow!("Unknown event type: {}", entry.event_type))?;
///         handler(entry, emit)
///     }
/// }
/// ```
pub trait OutboxEventRegistry: Send + Sync {
    /// Deserialize an outbox entry and emit it via the callback.
    ///
    /// Returns an error if:
    /// - The event type is not registered
    /// - Deserialization fails
    fn emit_entry(&self, entry: &OutboxEntry, emit: &EmitCallback) -> Result<()>;
}

// =============================================================================
// OutboxPublisherConfig
// =============================================================================

/// Configuration for the outbox publisher.
#[derive(Debug, Clone)]
pub struct OutboxPublisherConfig {
    /// How often to poll for unpublished events.
    pub poll_interval: std::time::Duration,
    /// Maximum events to process per poll.
    pub batch_size: usize,
    /// How long to keep published events before cleanup.
    pub retention: std::time::Duration,
    /// How often to run cleanup.
    pub cleanup_interval: std::time::Duration,
}

impl Default for OutboxPublisherConfig {
    fn default() -> Self {
        Self {
            poll_interval: std::time::Duration::from_millis(100),
            batch_size: 100,
            retention: std::time::Duration::from_secs(7 * 24 * 60 * 60), // 7 days
            cleanup_interval: std::time::Duration::from_secs(60 * 60),   // 1 hour
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_correlation_id_none() {
        let cid = CorrelationId::NONE;
        assert!(cid.is_none());
        assert!(!cid.is_some());
        assert_eq!(cid.into_inner(), Uuid::nil());
    }

    #[test]
    fn test_correlation_id_new() {
        let cid = CorrelationId::new();
        assert!(!cid.is_none());
        assert!(cid.is_some());
        assert_ne!(cid.into_inner(), Uuid::nil());
    }

    #[test]
    fn test_correlation_id_from_uuid() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);
        assert_eq!(cid.into_inner(), uuid);
    }

    #[test]
    fn test_correlation_id_from_option_some() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(Some(uuid));
        assert!(!cid.is_none());
        assert_eq!(cid.into_inner(), uuid);
    }

    #[test]
    fn test_correlation_id_from_option_none() {
        let cid = CorrelationId::from(None::<Uuid>);
        assert!(cid.is_none());
        assert_eq!(cid.into_inner(), Uuid::nil());
    }

    #[test]
    fn test_correlation_id_display() {
        let cid = CorrelationId::NONE;
        assert_eq!(format!("{}", cid), "NONE");

        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);
        assert_eq!(format!("{}", cid), format!("{}", uuid));
    }

    #[test]
    fn test_correlation_id_default() {
        let cid = CorrelationId::default();
        assert!(!cid.is_none()); // default creates a new random ID
    }

    #[test]
    fn test_outbox_publisher_config_default() {
        let config = OutboxPublisherConfig::default();
        assert_eq!(config.poll_interval, std::time::Duration::from_millis(100));
        assert_eq!(config.batch_size, 100);
        assert_eq!(
            config.retention,
            std::time::Duration::from_secs(7 * 24 * 60 * 60)
        );
        assert_eq!(
            config.cleanup_interval,
            std::time::Duration::from_secs(60 * 60)
        );
    }
}
