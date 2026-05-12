//! Projection configuration types and persistence trait.
//!
//! This module defines the public API for configuring projections (mode,
//! retry policy, start position) and the persistence trait
//! (`ProjectionStore`) that backends implement to support per-projection
//! cursors. The runtime (`ProjectionRunner`, engine integration,
//! sync-vs-async dispatch) is implemented separately and consumes this
//! trait — see `docs/plans/2026-05-04-feat-async-projections-plan.md`.
//!
//! Backends implement `ProjectionStore` against their own storage. An
//! in-memory implementation is provided on `MemoryStore` for tests and
//! single-process use cases.
//!
//! ## Recommended schema (Postgres backends)
//!
//! ```sql
//! CREATE TABLE causal_projection_cursors (
//!     projection_id        TEXT PRIMARY KEY,
//!     cursor_position      BIGINT NOT NULL,
//!     paused               BOOL NOT NULL DEFAULT FALSE,
//!     last_error           TEXT,
//!     last_attempt_at      TIMESTAMPTZ,
//!     consecutive_failures INT NOT NULL DEFAULT 0,
//!     -- Forward-compat columns for future multi-process leases (D3
//!     -- in the plan). Unused in 0.3; reserved so adding leases later
//!     -- is a column-population change, not a schema migration.
//!     leased_by            TEXT,
//!     leased_until         TIMESTAMPTZ,
//!     fencing_token        BIGINT
//! );
//!
//! CREATE TABLE causal_projection_failures (
//!     projection_id  TEXT NOT NULL,
//!     event_id       UUID NOT NULL,
//!     error          TEXT NOT NULL,
//!     attempts       INT NOT NULL,
//!     failed_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
//!     PRIMARY KEY (projection_id, event_id)
//! );
//! ```
//!
//! The DLQ table's primary key on `(projection_id, event_id)` is
//! load-bearing — it makes `advance_past_failure` idempotent on retry.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::LogCursor;

// ─────────────────────────────────────────────────────────────────────────
// Configuration types (D1, D4, D5)
// ─────────────────────────────────────────────────────────────────────────

/// Whether a projection runs inline with dispatch (`Sync`) or in an
/// independent runner with its own cursor (`Async`).
///
/// **D1: no default.** Every `register_projection` call site declares
/// its mode explicitly. Both extremes — defaulting `Sync` (preserves
/// the pre-0.2.0 silent-coupling bug for new projections) and
/// defaulting `Async` (silently breaks read-your-writes consumers) —
/// are wrong; the API refuses to choose so the caller has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionMode {
    /// Runs inline in the dispatch loop, before reactors. Failure
    /// blocks the dispatch cursor; retry budget then park applies.
    /// Read-your-writes within the same causal chain.
    Sync,
    /// Runs in an independent `ProjectionRunner` with its own cursor.
    /// Failure does not block dispatch. Lag is observable; recovery
    /// is per-projection.
    Async,
}

/// Backoff policy between retry attempts on projection failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// No delay between attempts.
    None,
    /// Constant delay between attempts.
    Linear { base: Duration },
    /// `min(max, base * 2^attempt)` with optional jitter. Default for
    /// async projections.
    Exponential {
        base: Duration,
        max: Duration,
        jitter: bool,
    },
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::Exponential {
            base: Duration::from_millis(100),
            max: Duration::from_secs(30),
            jitter: true,
        }
    }
}

/// What happens when a projection keeps failing.
///
/// **D4: default `BlockUntilFixed`.** Preserves the event-sourcing
/// invariant (projection state == fold(log)). Switching
/// `BlockUntilFixed → AdvanceAfter` is reversible at runtime; switching
/// the other direction doesn't recover lost events. Default to the
/// recoverable choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureBehavior {
    /// Retry forever with backoff. Cursor does not advance past the
    /// failing event until the projection succeeds or an operator
    /// intervenes. Lag grows visibly while stuck.
    BlockUntilFixed,
    /// Retry `max_attempts` times, then advance the cursor past the
    /// failed event and record a per-projection DLQ row. Operator
    /// manually replays via `reset_projection(id, position)` after
    /// fixing the issue.
    AdvanceAfter { max_attempts: u32 },
}

impl Default for FailureBehavior {
    fn default() -> Self {
        FailureBehavior::BlockUntilFixed
    }
}

/// Combined retry policy for an async projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryPolicy {
    pub backoff: Backoff,
    pub failure: FailureBehavior,
}

/// Where an async projection should start when its runner first comes
/// up.
///
/// **D5: no default.** Refusing to default forces a per-site decision
/// the same way mode does. The "obvious" defaults are both wrong in
/// some case: `ResumeOrLatest` silently makes historical events
/// invisible to a freshly-added projection; `Zero` is catastrophically
/// expensive for redeploys of long-running projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPosition {
    /// Use the existing cursor if one is persisted for this
    /// `projection_id`; otherwise start at the current
    /// `latest_position()`. Pragmatic production choice for projections
    /// that already have history.
    ResumeOrLatest,
    /// Always start at the current `latest_position()`, ignoring any
    /// persisted cursor. New projection that should NOT backfill.
    Latest,
    /// Always start at `LogCursor::ZERO`. Force backfill from the
    /// beginning of the event log. Search-index rebuild, analytics
    /// reset, etc.
    Zero,
    /// Specific position. Manual rewind for debugging or partial
    /// replay.
    Specific(LogCursor),
}

// ─────────────────────────────────────────────────────────────────────────
// Status + DLQ row types
// ─────────────────────────────────────────────────────────────────────────

/// Status snapshot of a projection.
///
/// Returned by `ProjectionStore::projection_status` and
/// `Engine::projection_status`. Used by the inspector UI and
/// operational tooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionStatus {
    pub projection_id: String,
    pub cursor: LogCursor,
    pub paused: bool,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
}

/// One DLQ row recorded by `advance_past_failure` when an async
/// projection in `FailureBehavior::AdvanceAfter` mode skips an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailure {
    pub projection_id: String,
    pub event_id: Uuid,
    pub error: String,
    pub attempts: u32,
    pub failed_at: DateTime<Utc>,
}

// ─────────────────────────────────────────────────────────────────────────
// v0.4 surface — minimal ops trait on top of CheckpointStore
// ─────────────────────────────────────────────────────────────────────────

/// Optional operational extension to [`CheckpointStore`]. Carries the
/// DLQ + pause/resume + status surface needed by ops tooling and the
/// inspector UI. Backends that don't need these — pure read-only
/// projections, throwaway tests — can satisfy the runtime with just
/// `CheckpointStore`.
///
/// Compared to the legacy [`ProjectionStore`]:
/// - Cursor get/set lives on `CheckpointStore` (required base trait).
/// - CAS variants (`advance_projection_cursor`, `advance_past_failure`)
///   are gone — the v0.4 runners use `CheckpointStore::set` and call
///   `record_failure` separately when DLQ-skipping is wanted.
/// - The trait surface drops from ~12 methods to 5.
///
/// ## Semantic change from `ProjectionStore`: DLQ write is no longer
/// atomic with cursor advance
///
/// The legacy [`ProjectionStore::advance_past_failure`] bundled the
/// DLQ row write and cursor advance into one transaction. A runner
/// crash between the two writes was impossible by construction.
///
/// `ProjectionOps` decouples them: callers in `AdvanceAfter` mode
/// invoke `record_failure(...)` and then `CheckpointStore::set(...)`
/// as **two separate writes**. A crash between them leaves a DLQ row
/// recorded but the cursor unmoved — the runner will retry the
/// failing event on restart and (if it succeeds this time) leave a
/// misleading DLQ entry for an event that ultimately processed.
///
/// **Implementation guidance for backends with transactions**:
/// expose your own atomic helper alongside the trait if you want the
/// legacy guarantee back. Backends without transactions (in-memory,
/// some KV stores) can't offer atomicity here anyway — the trait's
/// looser contract reflects what's actually portable.
///
/// During the v0.3 → v0.4 transition this trait is available alongside
/// `ProjectionStore`; P11 (Legacy collapse) deletes `ProjectionStore`
/// and migrates Postgres backends to `ProjectionOps` directly.
#[async_trait]
pub trait ProjectionOps: crate::checkpoint_store::CheckpointStore {
    /// Set the paused flag for a projection. Runners check this
    /// before each batch and skip work while `paused = true`.
    async fn set_paused(&self, group_name: &str, paused: bool) -> Result<()>;

    /// Record a DLQ row for a failed event. Does NOT advance the
    /// cursor — that's `CheckpointStore::set`'s job. Caller decides
    /// the order (skip-after vs block-on). Idempotent on
    /// `(group_name, event_id)` per the underlying primary-key
    /// contract.
    async fn record_failure(
        &self,
        group_name: &str,
        event_id: Uuid,
        error: &str,
        attempts: u32,
    ) -> Result<()>;

    /// List DLQ rows for a projection, most-recent first, limited to
    /// `limit` rows.
    async fn list_failures(
        &self,
        group_name: &str,
        limit: usize,
    ) -> Result<Vec<ProjectionFailure>>;

    /// Return the operational status of a projection. Returns `None`
    /// if no cursor exists for this id.
    async fn status(&self, group_name: &str) -> Result<Option<ProjectionStatus>>;

    /// Delete one DLQ row. Used by operator workflows that fix the
    /// underlying bug then remove the failure record. Returns `true`
    /// if a row was deleted, `false` if no row matched.
    async fn delete_failure(
        &self,
        group_name: &str,
        event_id: Uuid,
    ) -> Result<bool>;
}
