//! `Materializer` trait — v0.3 idempotent at-least-once external state.
//!
//! A Materializer applies a fact to external state (Postgres tables,
//! Neo4j graph, search index, foreign API) idempotently keyed on
//! `ctx.event_id`. The runtime delivers each matching fact at-least-once;
//! the application's idempotency turns that into exactly-once observed
//! effect (per C8 in the v0.3 API design plan).
//!
//! Materializers are NOT allowed to read Views (per C13). All derived
//! state needed by a materializer must be folded directly from the
//! facts the materializer subscribes to.
//!
//! There is no `commit_reactor_batch`-style atomicity here: materializer
//! cursor advance is per-fact (per C2), independent of the writes
//! `materialize()` performs. Crash between successful `materialize()`
//! return and cursor checkpoint causes the same fact to redeliver,
//! which idempotency absorbs.

use anyhow::Result;
use async_trait::async_trait;

use crate::contexts::Ctx;
use crate::fact::Fact;

#[async_trait]
pub trait Materializer: Send + Sync {
    type Fact: Fact;

    /// Cross-consumer dependency declaration. The runner refuses to
    /// advance this materializer's cursor past position P until every
    /// id in `DEPENDS_ON` has cursor ≥ P. Defaults to no deps.
    /// (See C2b — `depends_on` fence.)
    const DEPENDS_ON: &'static [&'static str] = &[];

    /// Apply a fact to external state. MUST be idempotent on
    /// `ctx.event_id` — the runtime calls at-least-once and relies
    /// on caller idempotency to prevent duplicate effects.
    async fn materialize(
        &self,
        fact: &Self::Fact,
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

    impl Fact for Recorded {
        const CATEGORY: &'static str = "records";
        fn name(&self) -> &str { "recorded" }
        fn stream_id(&self) -> Uuid { self.id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    /// Mock materializer that counts apply calls. Idempotent on event_id —
    /// a real materializer would use ON CONFLICT or MERGE-on-id.
    struct CountingSink {
        seen:  Arc<dashmap::DashSet<Uuid>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Materializer for CountingSink {
        type Fact = Recorded;

        async fn materialize(
            &self,
            fact: &Recorded,
            ctx: Ctx<'_>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Idempotent: insert returns false if already present.
            self.seen.insert(ctx.event_id);
            // Touch the fact to satisfy the borrow check in tests.
            assert_eq!(fact.occurred_at, ctx.occurred_at);
            Ok(())
        }
    }

    #[tokio::test]
    async fn materialize_runs_for_a_fact() {
        let sink = CountingSink {
            seen:  Arc::new(dashmap::DashSet::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let meta = Metadata::new();
        let event_id = Uuid::new_v4();
        let occurred = Utc::now();

        sink.materialize(
            &Recorded { id: event_id, occurred_at: occurred },
            Ctx {
                event_id,
                log_position:   LogCursor::ZERO,
                occurred_at:    occurred,
                correlation_id: Uuid::nil(),
                metadata:       &meta,
                aggregators:    None,
            },
        ).await.unwrap();

        assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
        assert!(sink.seen.contains(&event_id));
    }

    #[tokio::test]
    async fn materialize_redelivery_idempotent_on_event_id() {
        // Simulates the at-least-once redelivery contract: same event_id
        // delivered twice; the materializer absorbs the duplicate.
        let sink = CountingSink {
            seen:  Arc::new(dashmap::DashSet::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let meta = Metadata::new();
        let event_id = Uuid::new_v4();
        let occurred = Utc::now();
        let fact = Recorded { id: event_id, occurred_at: occurred };

        for _ in 0..2 {
            sink.materialize(
                &fact,
                Ctx {
                    event_id,
                    log_position:   LogCursor::ZERO,
                    occurred_at:    occurred,
                    correlation_id: Uuid::nil(),
                    metadata:       &meta,
                    aggregators:    None,
                },
            ).await.unwrap();
        }

        // calls = 2 (runtime delivered twice) but seen has only one entry.
        assert_eq!(sink.calls.load(Ordering::SeqCst), 2);
        assert_eq!(sink.seen.len(), 1);
    }
}
