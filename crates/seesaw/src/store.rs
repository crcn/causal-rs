//! Pluggable backend trait for the Engine's settle loop.
//!
//! The [`Store`] trait abstracts event and handler queue operations so that
//! the Engine can run against different backends (in-memory, Postgres, etc.).
//!
//! # Handler lifecycle
//!
//! Each handler execution follows a state machine driven by the Engine's
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
//!                               │ poll_next_handler
//!                               ▼
//!                  ┌──────────────────────────┐
//!                  │         running           │
//!                  │  (claimed by Engine)      │
//!                  └──┬────────┬────────┬─────┘
//!                     │        │        │
//!        resolve_handler(Complete/Retry/DeadLetter)
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
    EventOutcome, ExpiredJoinWindow, HandlerResolution, JoinAppendParams,
    JoinEntry, JournalEntry, NewEvent, PersistedEvent, QueueStatus, QueuedEvent, QueuedHandler,
    Snapshot,
};

/// Pluggable backend for the Engine's event and handler queues.
///
/// Implementations must be `Send + Sync` so the Engine can be shared
/// across tasks. All methods are async and fallible — a Postgres
/// implementation can surface connection errors instead of silently
/// dropping work.
///
/// See the [module-level docs](crate::store) for the handler lifecycle
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
    /// - `Processed`: ack source event, persist handler intents (pending state),
    ///   DLQ projection failures.
    /// - `Rejected`: send to DLQ and ack in one atomic step.
    async fn complete_event(&self, result: EventOutcome) -> Result<()>;

    // ── Handler queue ─────────────────────────────────────────────────

    /// Claim the next ready handler execution.
    ///
    /// Transitions the handler from **pending** → **running**.
    ///
    /// For Postgres this is typically `SELECT ... FOR UPDATE SKIP LOCKED`
    /// with a status flip. If the worker crashes before calling a
    /// resolution method, the Store is responsible for reclaiming the
    /// handler (e.g. via a visibility timeout that resets status back to
    /// pending).
    async fn poll_next_handler(&self) -> Result<Option<QueuedHandler>>;

    /// Returns the earliest `execute_at` of any **pending** handler, if any.
    ///
    /// Used by the settle loop to sleep until the next handler becomes ready
    /// instead of exiting prematurely when only future-dated handlers remain.
    async fn earliest_pending_handler_at(&self) -> Result<Option<DateTime<Utc>>>;

    // ── Handler resolution ─────────────────────────────────────────────

    /// Resolve a **running** handler.
    ///
    /// This is the single exit point for every claimed handler:
    /// - `Complete`: mark done, publish emitted events, clear journal entries
    /// - `Retry`: reset to pending with new attempt count and schedule
    /// - `DeadLetter`: mark dead-lettered, publish terminal events
    ///
    /// On `Complete`, implementations should clear journal entries for the
    /// handler's `(handler_id, event_id)` atomically with the completion.
    async fn resolve_handler(&self, resolution: HandlerResolution) -> Result<()>;

    /// Reclaim handlers that have been in "running" state longer than their timeout.
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

    // ── Handler journaling (optional) ─────────────────────────────
    //
    // Override to enable `ctx.run()` replay on retry. Journal entries
    // record the serialized result of each `ctx.run()` call so that
    // retried handlers can skip already-completed steps.

    /// Load all journal entries for a handler execution.
    async fn load_journal(
        &self,
        _handler_id: &str,
        _event_id: Uuid,
    ) -> Result<Vec<JournalEntry>> {
        Ok(Vec::new())
    }

    /// Append a single journal entry (one `ctx.run()` result).
    async fn append_journal(
        &self,
        _handler_id: &str,
        _event_id: Uuid,
        _seq: u32,
        _value: serde_json::Value,
    ) -> Result<()> {
        Ok(())
    }

    /// Clear journal entries after successful handler completion.
    async fn clear_journal(
        &self,
        _handler_id: &str,
        _event_id: Uuid,
    ) -> Result<()> {
        Ok(())
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
