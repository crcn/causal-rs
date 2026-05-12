//! Per-consumer cursor storage + reactor outbox traits.
//!
//! - [`CheckpointStore`] — minimal cursor read/write. Required by
//!   `Projector` / `MultiProjector` / `Reactor` runners.
//! - [`ReactorOutbox`] — extends `CheckpointStore` with the atomic
//!   commit primitive that codifies C12 (runtime-side outbox + cursor
//!   advance in one transaction). Required only for engines hosting
//!   reactors.
//!
//! Backends implement these directly. The legacy
//! `impl<T: ProjectionStore> CheckpointStore for T` blanket is gone
//! as of P11.e — backends must provide a direct `CheckpointStore`
//! impl.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::LogCursor;

/// Minimal cursor read/write surface.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>>;
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────
// ReactorOutbox — atomic outbox + cursor advance (C12)
// ─────────────────────────────────────────────────────────────────────

/// Caller-supplied outbox row for `commit_reactor_batch`. Same fields
/// as [`OutboxRow`] minus `id` and `created_at` — those are
/// backend-assigned (BIGSERIAL / atomic counter for `id`; `now()` /
/// `Utc::now()` for `created_at`). Splitting input from read-shape
/// avoids the awkward "pass placeholder values that get overwritten"
/// pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertableOutboxRow {
    pub reactor_id:      String,
    pub source_event_id: Uuid,
    pub output_index:    u32,
    pub event_id:        Uuid,
    pub event_type:      String,
    pub fact_payload:    serde_json::Value,
    pub correlation_id:  Uuid,
}

/// Persisted outbox row returned by `outbox_pending`. Includes the
/// backend-assigned `id` and `created_at`.
///
/// `event_id` is derived deterministically by the runtime from
/// `(reactor_id, source_event_id, output_index)` so retried reactor
/// runs collapse into a single log entry via the log's idempotent-
/// append-on-event-id contract (C1).
///
/// `event_type` and `correlation_id` are carried so the relay can
/// reconstruct a `NewEvent` for `EventLogBackend::append` without
/// re-deserializing the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id:              i64,
    pub reactor_id:      String,
    pub source_event_id: Uuid,
    pub output_index:    u32,
    pub event_id:        Uuid,
    pub event_type:      String,
    pub fact_payload:    serde_json::Value,
    pub correlation_id:  Uuid,
    pub created_at:      DateTime<Utc>,
}

/// Reactor outbox storage. Extends `CheckpointStore` with the atomic
/// commit primitive that backs C12, plus the DLQ attempt-counter
/// surface that lets `ReactorRunner` track retries across step
/// boundaries — and across engine restarts when backed by a durable
/// store.
#[async_trait]
pub trait ReactorOutbox: CheckpointStore {
    /// Atomically: insert N outbox rows AND optionally advance a
    /// cursor. Backends MUST commit both in one transaction; partial
    /// observable state is a contract violation.
    ///
    /// `cursor` is `Some((consumer_id, pos))` for the typical reactor
    /// step (commit a batch of outputs and advance the reactor's
    /// cursor); `None` for backfill scenarios.
    ///
    /// Backend assigns `id` and `created_at` — caller's
    /// [`InsertableOutboxRow`] doesn't carry them.
    async fn commit_reactor_batch(
        &self,
        rows: Vec<InsertableOutboxRow>,
        cursor: Option<(String, LogCursor)>,
    ) -> Result<()>;

    /// Drain candidates for the relay loop. Order: oldest first by
    /// `created_at`, ties broken by `id`. Caller MAY call repeatedly;
    /// the same row may appear in subsequent calls until
    /// `outbox_delete` removes it.
    async fn outbox_pending(&self, limit: usize) -> Result<Vec<OutboxRow>>;

    /// Mark a row delivered. Idempotent: deleting an already-deleted
    /// id MUST succeed (Ok), as the relay may retry after a partial
    /// crash.
    async fn outbox_delete(&self, id: i64) -> Result<()>;

    /// Increment the attempt counter for a `(consumer_id,
    /// source_event_id)` pair and return the new count. Called by
    /// `ReactorRunner` on every `react()` failure to track retries
    /// for the DLQ path. Idempotent semantics — backends with
    /// transactions SHOULD `INSERT ... ON CONFLICT DO UPDATE SET
    /// count = count + 1 RETURNING count`. The returned value is the
    /// count INCLUDING this attempt (first failure returns 1).
    ///
    /// **Persistence contract**: durable backends MUST persist this
    /// across process restarts. In-memory backends (MemoryStore)
    /// trivially persist within the store's lifetime; process crash
    /// loses state, which matches MemoryStore's documented "no
    /// durability" position.
    async fn record_reactor_attempt(
        &self,
        consumer_id: &str,
        source_event_id: Uuid,
    ) -> Result<u32>;

    /// Clear the attempt counter for a `(consumer_id,
    /// source_event_id)` pair. Called on successful `react()` (the
    /// next failure should start fresh) and after the DLQ mapper
    /// has fired (the next time this event is seen — if ever — is
    /// a fresh decision). Idempotent.
    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        source_event_id: Uuid,
    ) -> Result<()>;
}
