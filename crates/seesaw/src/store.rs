//! Pluggable backend trait for the Engine's settle loop.
//!
//! The [`Store`] trait abstracts event and effect queue operations so that
//! the Engine can run against different backends (in-memory, Postgres, etc.).
//!
//! # Effect lifecycle
//!
//! Each effect execution follows a state machine driven by the Engine's
//! calls into the Store. Implementations are free to track these states
//! (e.g. as a `status` column) for observability, health checks, or
//! distributed reclaim logic.
//!
//! ```text
//!                  ┌──────────────────────────┐
//!                  │         pending           │
//!                  │  (complete_event inserts   │
//!                  │   the intent)              │
//!                  └────────────┬─────────────┘
//!                               │ poll_next_effect
//!                               ▼
//!                  ┌──────────────────────────┐
//!                  │         running           │
//!                  │  (claimed by Engine)      │
//!                  └──┬────────┬────────┬─────┘
//!                     │        │        │
//!          resolve_effect(Complete/Retry/DeadLetter)
//!                     │        │        │
//!                     ▼        ▼        ▼
//!               ┌──────────┐ ┌────┐ ┌──────────────┐
//!               │completed │ │pending│ │dead_lettered │
//!               │          │ │(retry)│ │              │
//!               └──────────┘ └──────┘ └──────────────┘
//! ```
//!
//! The trait defines only the **write** path (state transitions). A
//! concrete Store implementation (e.g. `PostgresStore`) can expose its
//! own query methods — `list_running()`, `count_by_status()`, etc. —
//! as inherent methods on the struct, outside the trait.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::{
    EffectResolution, EventOutcome, ExpiredJoinWindow, JoinAppendParams,
    JoinEntry, NewEvent, PersistedEvent, QueueStatus, QueuedEvent, QueuedEffect, Snapshot,
};

/// Pluggable backend for the Engine's event and effect queues.
///
/// Implementations must be `Send + Sync` so the Engine can be shared
/// across tasks. All methods are async and fallible — a Postgres
/// implementation can surface connection errors instead of silently
/// dropping work.
///
/// See the [module-level docs](crate::store) for the effect lifecycle
/// state machine.
#[async_trait]
pub trait Store: Send + Sync {
    // ── Event queue ──────────────────────────────────────────────────

    /// Enqueue an event for processing.
    async fn publish(&self, event: QueuedEvent) -> Result<()>;

    /// Claim the next ready event (FIFO per correlation_id).
    async fn poll_next(&self) -> Result<Option<QueuedEvent>>;

    /// Complete event processing — either commit results or reject to DLQ.
    ///
    /// - `Processed`: ack source event, persist effect intents (pending state),
    ///   publish inline-emitted events, DLQ inline failures.
    /// - `Rejected`: send to DLQ and ack in one atomic step.
    async fn complete_event(&self, result: EventOutcome) -> Result<()>;

    // ── Effect queue ─────────────────────────────────────────────────

    /// Claim the next ready effect execution.
    ///
    /// Transitions the effect from **pending** → **running**.
    ///
    /// For Postgres this is typically `SELECT ... FOR UPDATE SKIP LOCKED`
    /// with a status flip. If the worker crashes before calling a
    /// resolution method, the Store is responsible for reclaiming the
    /// effect (e.g. via a visibility timeout that resets status back to
    /// pending).
    async fn poll_next_effect(&self) -> Result<Option<QueuedEffect>>;

    /// Returns the earliest `execute_at` of any **pending** effect, if any.
    ///
    /// Used by the settle loop to sleep until the next effect becomes ready
    /// instead of exiting prematurely when only future-dated effects remain.
    async fn earliest_pending_effect_at(&self) -> Result<Option<DateTime<Utc>>>;

    // ── Effect resolution ─────────────────────────────────────────────

    /// Resolve a **running** effect.
    ///
    /// This is the single exit point for every claimed effect:
    /// - `Complete`: mark done, publish emitted events
    /// - `Retry`: reset to pending with new attempt count and schedule
    /// - `DeadLetter`: mark dead-lettered, publish terminal events
    async fn resolve_effect(&self, resolution: EffectResolution) -> Result<()>;

    /// Reclaim effects that have been in "running" state longer than their timeout.
    /// Called at the start of each settle loop iteration.
    /// Default: no-op (suitable for in-memory stores where crash = data loss anyway).
    async fn reclaim_stale(&self) -> Result<()> {
        Ok(())
    }

    // ── Join windows ─────────────────────────────────────────────────

    /// Append an entry to a join window. If the window is now complete,
    /// atomically claim it and return the ordered entries.
    async fn join_append_and_maybe_claim(
        &self,
        params: JoinAppendParams,
    ) -> Result<Option<Vec<JoinEntry>>>;

    /// Mark a join window as completed (all processing done).
    async fn join_complete(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
    ) -> Result<()>;

    /// Release a claimed join window back to Open state (on handler error).
    async fn join_release(
        &self,
        join_handler_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
        error: String,
    ) -> Result<()>;

    /// Expire open join windows whose timeout has passed. Returns metadata
    /// for each expired window so the Engine can DLQ the source events.
    async fn expire_join_windows(&self, now: DateTime<Utc>) -> Result<Vec<ExpiredJoinWindow>>;

    // ── Event persistence (optional) ──────────────────────────────
    //
    // Override to enable durable event log, aggregate hydration, and
    // cross-node sync. Default no-ops make simple in-memory stores
    // work without implementing persistence.

    /// Append a single event to the global log. Returns global position.
    ///
    /// **Idempotency contract:** If an event with the same `event_id`
    /// already exists, return the existing position without inserting.
    async fn append_event(&self, _event: NewEvent) -> Result<u64> {
        Ok(0)
    }

    /// Load events for an aggregate stream (for hydration).
    ///
    /// Pass `after_position: Some(pos)` for snapshot + partial replay,
    /// or `None` for full replay.
    async fn load_stream(
        &self,
        _aggregate_type: &str,
        _aggregate_id: Uuid,
        _after_position: Option<u64>,
    ) -> Result<Vec<PersistedEvent>> {
        Ok(Vec::new())
    }

    /// Load events from the global log after a given position.
    ///
    /// Returns up to `limit` events with `position > after_position`,
    /// ordered by position. Used for tailing the global log across nodes.
    async fn load_global_from(
        &self,
        _after_position: u64,
        _limit: usize,
    ) -> Result<Vec<PersistedEvent>> {
        Ok(Vec::new())
    }

    // ── Snapshots (optional) ──────────────────────────────────────
    //
    // Override to accelerate cold-start hydration.

    /// Load the latest snapshot for an aggregate.
    async fn load_snapshot(
        &self,
        _aggregate_type: &str,
        _aggregate_id: Uuid,
    ) -> Result<Option<Snapshot>> {
        Ok(None)
    }

    /// Save a snapshot of aggregate state.
    async fn save_snapshot(&self, _snapshot: Snapshot) -> Result<()> {
        Ok(())
    }

    // ── Cancellation (optional) ───────────────────────────────────

    /// Mark a correlation ID as cancelled.
    ///
    /// Best-effort "stop-at-next-checkpoint": handlers already mid-execution
    /// will complete, but their output events will be rejected. Side effects
    /// (API calls, emails) from in-flight handlers cannot be undone.
    async fn cancel_correlation(&self, _correlation_id: Uuid) -> Result<()> {
        Ok(())
    }

    /// Check whether a correlation ID has been cancelled.
    async fn is_cancelled(&self, _correlation_id: Uuid) -> Result<bool> {
        Ok(false)
    }

    // ── Queue status (optional) ──────────────────────────────────

    /// Return a summary of pending work for a correlation ID.
    ///
    /// Default returns zeros (backward-compatible with stores that don't
    /// track queue depth).
    async fn queue_status(&self, _correlation_id: Uuid) -> Result<QueueStatus> {
        Ok(QueueStatus::default())
    }
}
