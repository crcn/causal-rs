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
//!                  │  (commit_event_processing │
//!                  │   inserts the intent)     │
//!                  └────────────┬─────────────┘
//!                               │ poll_next_effect
//!                               ▼
//!                  ┌──────────────────────────┐
//!                  │         running           │
//!                  │  (claimed by Engine)      │
//!                  └──┬────────┬────────┬─────┘
//!                     │        │        │
//!          complete_effect  fail_effect  dlq_effect
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
    EffectCompletion, EffectDlq, EventProcessingCommit, ExpiredJoinWindow, JoinAppendParams,
    JoinEntry, NewEvent, PersistedEvent, QueuedEvent, QueuedHandlerExecution, Snapshot,
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

    /// Atomically commit event processing results: ack the source event,
    /// persist effect intents, publish inline-emitted events, and DLQ
    /// inline failures.
    ///
    /// Effect intents inserted here start in the **pending** state.
    async fn commit_event_processing(&self, commit: EventProcessingCommit) -> Result<()>;

    /// Reject an event: send it to the DLQ and ack in one atomic step.
    async fn reject_event(
        &self,
        event_row_id: i64,
        event_id: Uuid,
        error: String,
        reason: String,
    ) -> Result<()>;

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
    async fn poll_next_effect(&self) -> Result<Option<QueuedHandlerExecution>>;

    /// Claim up to `limit` ready effect executions.
    ///
    /// Each claimed effect transitions from **pending** → **running**.
    ///
    /// Default implementation calls [`poll_next_effect`](Store::poll_next_effect)
    /// in a loop. Postgres can override with `SELECT ... LIMIT N FOR UPDATE SKIP LOCKED`.
    async fn poll_next_effects(&self, limit: usize) -> Result<Vec<QueuedHandlerExecution>> {
        let mut batch = Vec::with_capacity(limit);
        for _ in 0..limit {
            match self.poll_next_effect().await? {
                Some(exec) => batch.push(exec),
                None => break,
            }
        }
        Ok(batch)
    }

    /// Returns the earliest `execute_at` of any **pending** effect, if any.
    ///
    /// Used by the settle loop to sleep until the next effect becomes ready
    /// instead of exiting prematurely when only future-dated effects remain.
    async fn earliest_pending_effect_at(&self) -> Result<Option<DateTime<Utc>>>;

    // ── Effect outcomes ──────────────────────────────────────────────
    //
    // Each of these resolves a **running** effect. Exactly one must be
    // called for every execution returned by `poll_next_effect`.

    /// Resolve a **running** effect as **completed**.
    ///
    /// Publishes any emitted events atomically with the status change.
    async fn complete_effect(&self, completion: EffectCompletion) -> Result<()>;

    /// Resolve a **running** effect back to **pending** for retry.
    ///
    /// Backoff computation stays in the Engine; the Store just persists
    /// the new attempt count and schedule.
    ///
    /// `new_attempts` is the value to **set** on the execution (not an increment).
    /// The Engine computes `previous_attempts + 1` before calling this method.
    async fn fail_effect(
        &self,
        event_id: Uuid,
        handler_id: String,
        error: String,
        new_attempts: i32,
        next_execute_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Resolve a **running** effect as **dead-lettered**.
    ///
    /// Publishes any terminal events (from `on_failure`/`on_dlq` mappers)
    /// atomically with the status change.
    async fn dlq_effect(&self, dlq: EffectDlq) -> Result<()>;

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
    async fn load_stream(
        &self,
        _aggregate_type: &str,
        _aggregate_id: Uuid,
    ) -> Result<Vec<PersistedEvent>> {
        Ok(Vec::new())
    }

    /// Load events from after a specific position (for snapshot + partial replay).
    async fn load_stream_from(
        &self,
        _aggregate_type: &str,
        _aggregate_id: Uuid,
        _after_position: u64,
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
}
