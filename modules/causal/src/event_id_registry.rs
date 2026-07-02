//! `EventIdRegistry` — an authoritative, unbounded global index of appended
//! `event_id`s, so `Any` (idempotent) appends can recognize a redelivery no
//! matter how deep the original output is buried.
//!
//! ## Why this exists (A2)
//!
//! The Kurrent backend makes `Any` appends idempotent with a *scan-then-CAS*
//! over the stream's tail window (`max(4·batch, 64)` events). That window
//! catches a redelivery only while the original output is still near the
//! head. The 0.19 decision-record **completion path** re-appends a sealed
//! batch on redelivery — and by then the original outputs can be arbitrarily
//! deep, past the window, so Kurrent's dedup misses and the re-append lands a
//! duplicate (empirically proven against live Kurrent).
//!
//! Postgres, already a required backend, enforces `event_id` uniqueness
//! absolutely via a `UNIQUE`/`PK` constraint. This registry lifts that same
//! guarantee in front of Kurrent: a small PG-side table
//! (`causal_event_ids`) consulted on every `Any` append. It is authoritative
//! and unbounded, so it recognizes deep redeliveries the window cannot.
//!
//! ## Protocol (append-then-register)
//!
//! On an `Any` append the backend:
//! 1. [`lookup`](EventIdRegistry::lookup)s the batch ids.
//!    - **all present** → redelivery: return the stored coordinates, do not
//!      re-append.
//!    - **some present** → partial overlap: a hard error (ids must be
//!      all-new or all-already-persisted, matching `reconcile`).
//!    - **none present** → proceed.
//! 2. append to the log,
//! 3. [`register`](EventIdRegistry::register) the batch ids at the write's
//!    coordinates (first-write-wins).
//!
//! A crash between (2) and (3) leaves the ids unregistered; the backend's
//! existing tail-window scan still catches that redelivery *if* it is within
//! the window. The registry's job is to extend recognition **beyond** the
//! window for the common case where the prior append registered successfully.
//!
//! `causal` owns the trait and the reference [`InMemoryEventIdRegistry`];
//! `causal_replay` supplies the Postgres backend.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use crate::types::{LogCursor, StreamRevision};

/// A registered event: its id and the coordinates of the write that landed
/// it. The coordinates are the batch's [`WriteResult`](crate::types::WriteResult)
/// (its last event's position/revision) — all ids in one atomic batch share
/// them, matching what an `Any` append returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventIdEntry {
    pub event_id: Uuid,
    pub stream_position: LogCursor,
    pub stream_revision: StreamRevision,
}

/// Authoritative global index of appended `event_id`s. See module docs.
#[async_trait]
pub trait EventIdRegistry: Send + Sync {
    /// For each id in `event_ids`, the registered entry if present (else
    /// `None`), positionally aligned with the input.
    async fn lookup(&self, event_ids: &[Uuid]) -> Result<Vec<Option<EventIdEntry>>>;

    /// Register `entries`, **first-write-wins**: an id already present is
    /// left untouched (a racing/duplicate register must not overwrite the
    /// canonical coordinates). Idempotent.
    async fn register(&self, entries: &[EventIdEntry]) -> Result<()>;
}

/// How a batch stands relative to the registry — the classification an `Any`
/// append acts on. Mirrors the `reconcile` helper's semantics but sourced
/// from the authoritative registry rather than a bounded window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchPresence {
    /// No id is registered — a genuine new append.
    Absent,
    /// Every id is registered — a redelivery. Carries the stored coordinates
    /// of the batch's last id (the `WriteResult` to return).
    Redelivery { last: EventIdEntry },
    /// Some but not all ids are registered — event_ids must be all-new or
    /// all-already-persisted.
    PartialOverlap,
}

/// Classify a batch against the registry with one [`lookup`](EventIdRegistry::lookup).
pub async fn classify_batch<R: EventIdRegistry + ?Sized>(
    registry: &R,
    batch_ids: &[Uuid],
) -> Result<BatchPresence> {
    if batch_ids.is_empty() {
        return Ok(BatchPresence::Absent);
    }
    let found = registry.lookup(batch_ids).await?;
    let present = found.iter().filter(|e| e.is_some()).count();
    if present == 0 {
        Ok(BatchPresence::Absent)
    } else if present == batch_ids.len() {
        let last = found
            .last()
            .and_then(|e| *e)
            .expect("all present ⇒ last id has an entry");
        Ok(BatchPresence::Redelivery { last })
    } else {
        Ok(BatchPresence::PartialOverlap)
    }
}

/// In-memory [`EventIdRegistry`] for tests and single-process use.
#[derive(Default)]
pub struct InMemoryEventIdRegistry {
    inner: Mutex<HashMap<Uuid, EventIdEntry>>,
}

impl InMemoryEventIdRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EventIdRegistry for InMemoryEventIdRegistry {
    async fn lookup(&self, event_ids: &[Uuid]) -> Result<Vec<Option<EventIdEntry>>> {
        let map = self.inner.lock().unwrap();
        Ok(event_ids.iter().map(|id| map.get(id).copied()).collect())
    }

    async fn register(&self, entries: &[EventIdEntry]) -> Result<()> {
        let mut map = self.inner.lock().unwrap();
        for e in entries {
            map.entry(e.event_id).or_insert(*e); // first-write-wins
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pos: u64) -> EventIdEntry {
        EventIdEntry {
            event_id: Uuid::new_v4(),
            stream_position: LogCursor::from_raw(pos),
            stream_revision: StreamRevision::from_raw(pos),
        }
    }

    #[tokio::test]
    async fn absent_then_registered_then_redelivery() {
        let reg = InMemoryEventIdRegistry::new();
        let e = entry(5);
        assert_eq!(
            classify_batch(&reg, &[e.event_id]).await.unwrap(),
            BatchPresence::Absent
        );
        reg.register(&[e]).await.unwrap();
        assert_eq!(
            classify_batch(&reg, &[e.event_id]).await.unwrap(),
            BatchPresence::Redelivery { last: e }
        );
    }

    #[tokio::test]
    async fn partial_overlap_detected() {
        let reg = InMemoryEventIdRegistry::new();
        let a = entry(1);
        let b = entry(2);
        reg.register(&[a]).await.unwrap();
        assert_eq!(
            classify_batch(&reg, &[a.event_id, b.event_id]).await.unwrap(),
            BatchPresence::PartialOverlap
        );
    }

    #[tokio::test]
    async fn register_is_first_write_wins() {
        let reg = InMemoryEventIdRegistry::new();
        let id = Uuid::new_v4();
        let first = EventIdEntry {
            event_id: id,
            stream_position: LogCursor::from_raw(10),
            stream_revision: StreamRevision::from_raw(0),
        };
        let second = EventIdEntry { stream_position: LogCursor::from_raw(99), ..first };
        reg.register(&[first]).await.unwrap();
        reg.register(&[second]).await.unwrap();
        let got = reg.lookup(&[id]).await.unwrap()[0].unwrap();
        assert_eq!(got.stream_position, LogCursor::from_raw(10), "first write wins");
    }
}
