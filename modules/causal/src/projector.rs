//! `Projector` trait — idempotent at-least-once external state.
//!
//! A Projector applies a fact to external state (Postgres tables,
//! Neo4j graph, search index, foreign API) idempotently keyed on
//! `ctx.event_id`. The runtime delivers each matching fact at-least-once;
//! the application's idempotency turns that into exactly-once observed
//! effect (per C8).
//!
//! Projectors must NOT read derived state from other consumers (per
//! C13). All state needed by a projector must be folded directly from
//! the facts the projector subscribes to.
//!
//! There is no `commit_reactor_batch`-style atomicity here: projector
//! cursor advance is per-fact (per C2), independent of the writes
//! `project()` performs. Crash between successful `project()` return
//! and cursor checkpoint causes the same fact to redeliver, which
//! idempotency absorbs.
//!
//! Subscription model: a Projector is a **catch-up subscription** — the
//! runner reads the log from a client-managed cursor (`read_all` from the
//! position stored in `CheckpointStore`) and advances that cursor per
//! fact. `NAME` is the cursor key. (Kurrent's server-side
//! *persistent* subscriptions — server checkpoints, ack/nack, competing
//! consumers — are a possible future backend optimization, not what ships;
//! against Kurrent the cursor reads `$et-{CATEGORY}:*` / `$ce-{CATEGORY}`.)

use anyhow::Result;
use async_trait::async_trait;

use crate::contexts::Ctx;
use crate::event::Event;

#[async_trait]
pub trait Projector: Send + Sync {
    type Event: Event;

    /// Consumer group name. Used as the consumer's cursor key in
    /// `CheckpointStore` (the catch-up subscription's identity); on a
    /// Kurrent backend it also names the category/event-type stream the
    /// cursor reads (`$ce-{CATEGORY}` / `$et-{CATEGORY}:*`).
    ///
    /// **Uniqueness contract:**
    /// - Within one `EngineBuilder`: enforced. Builder panics on
    ///   duplicate registration.
    /// - Across engines sharing one `CheckpointStore`: **NOT
    ///   enforced by the framework.** Two engines concurrently
    ///   running with the same `NAME` against the same backend
    ///   will silently corrupt each other's cursors. Backends that
    ///   support advisory locks (Postgres) should acquire one keyed
    ///   on `NAME` at engine build time. Single-process
    ///   deployments (one engine per process) are unaffected.
    const NAME: &'static str;

    /// Cross-consumer dependency declaration. The runner refuses to
    /// advance this projector's cursor past position P until every
    /// id in `DEPENDS_ON` has cursor ≥ P. Defaults to no deps.
    /// (See C2b — `depends_on` fence.)
    const DEPENDS_ON: &'static [&'static str] = &[];

    /// Apply a fact to external state. MUST be idempotent on
    /// `ctx.event_id` — the runtime calls at-least-once and relies
    /// on caller idempotency to prevent duplicate effects.
    async fn project(
        &self,
        fact: &Self::Event,
        ctx: Ctx<'_>,
    ) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contexts::Metadata;
        use crate::types::LogCursor;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use uuid::Uuid;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Recorded {
        id: Uuid,
        occurred_at: DateTime<Utc>,
    }

    impl Event for Recorded {
        const NAME: &'static str = "recorded";
        fn subject_id(&self) -> Uuid { self.id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    /// Mock projector that counts apply calls. Idempotent on event_id —
    /// a real projector would use ON CONFLICT or MERGE-on-id.
    struct CountingSink {
        seen:  Arc<dashmap::DashSet<Uuid>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Projector for CountingSink {
        type Event = Recorded;
        const NAME: &'static str = "counting-sink";

        async fn project(
            &self,
            fact: &Recorded,
            ctx: Ctx<'_>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.insert(ctx.event_id);
            assert_eq!(fact.occurred_at, ctx.occurred_at);
            Ok(())
        }
    }

    #[tokio::test]
    async fn project_runs_for_a_fact() {
        let sink = CountingSink {
            seen:  Arc::new(dashmap::DashSet::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let meta = Metadata::new();
        let event_id = Uuid::new_v4();
        let occurred = Utc::now();

        sink.project(
            &Recorded { id: event_id, occurred_at: occurred },
            Ctx {
                event_id,
                log_position:   LogCursor::ZERO,
                occurred_at:    occurred,
                workflow_id: Uuid::nil(),
                metadata:       &meta,
                consumer: "",
                labels:   None,
                state:    crate::contexts::StateSource::None,
                logs:           None,
                effect_store: None,
                cancelled_workflows: None,
                fence_consulted: None,
            },
        ).await.unwrap();

        assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
        assert!(sink.seen.contains(&event_id));
    }

    #[tokio::test]
    async fn project_redelivery_idempotent_on_event_id() {
        let sink = CountingSink {
            seen:  Arc::new(dashmap::DashSet::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let meta = Metadata::new();
        let event_id = Uuid::new_v4();
        let occurred = Utc::now();
        let fact = Recorded { id: event_id, occurred_at: occurred };

        for _ in 0..2 {
            sink.project(
                &fact,
                Ctx {
                    event_id,
                    log_position:   LogCursor::ZERO,
                    occurred_at:    occurred,
                    workflow_id: Uuid::nil(),
                    metadata:       &meta,
                    consumer: "",
                    labels:   None,
                    state:    crate::contexts::StateSource::None,
                    logs:           None,
                    effect_store: None,
                    cancelled_workflows: None,
                    fence_consulted: None,
                },
            ).await.unwrap();
        }

        assert_eq!(sink.calls.load(Ordering::SeqCst), 2);
        assert_eq!(sink.seen.len(), 1);
    }
}
