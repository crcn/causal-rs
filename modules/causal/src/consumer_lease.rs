//! Exclusive consumer lease — prevents two servers from processing the
//! same consumer's triggers simultaneously.
//!
//! Before a `ReactorRunner` starts processing triggers it acquires an
//! exclusive lease for its `consumer_id` via [`ConsumerLeasor::acquire`].
//! The returned [`LeaseGuard`] is held for the runner's lifetime; when the
//! runner halts (and the `Core` drops), the guard drops — releasing the
//! lease and allowing another server to take over.
//!
//! The canonical production implementation is `PgConsumerLeasor` (in
//! `causal_replay`), which uses Postgres session-level advisory locks so
//! the OS reclaims the lock automatically on process crash — no separate
//! heartbeat or TTL needed. Tests can supply a mock implementation
//! without any infrastructure.

use anyhow::Result;
use async_trait::async_trait;

/// An exclusive hold on a named consumer slot.
///
/// The lease is released when this guard is dropped. Implementations
/// that wrap a Postgres connection need only drop the connection —
/// Postgres releases all session advisory locks when the connection
/// closes.
pub trait LeaseGuard: Send + Sync {}

/// Acquires an exclusive lease for a consumer before it begins processing.
///
/// Multiple calls with the same `consumer_id` block until the current
/// holder drops (or the underlying resource — e.g. the DB connection —
/// is closed by a crash). The returned guard holds the lease for its
/// entire lifetime.
#[async_trait]
pub trait ConsumerLeasor: Send + Sync {
    async fn acquire(&self, consumer_id: &str) -> Result<Box<dyn LeaseGuard>>;
}
