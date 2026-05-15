//! `Aggregate` marker + `Apply<F: Event>` extension.
//!
//! Aggregates are the consistency boundary on the write path — the
//! place where command-time invariants are enforced before facts hit
//! the log. Per C6, `Engine::append<A, F>` is OCC-protected: command
//! handlers `load<A, F>` to fold the stream, decide, then
//! `append<A, F>` with `expected_version`; the backend errors with
//! `Conflict` if the stream moved.
//!
//! Per C11, Reactors cannot emit into Aggregate streams; saga-shaped
//! "emit only if aggregate at version V" operations must be modeled
//! as command handlers. `EngineBuilder::with_aggregate<A, F>`
//! registers `F::CATEGORY` as `StreamPolicy::OccRequired` so
//! `Engine::emit` rejects writes to it; `load<A, F>` / `append<A, F>`
//! do the OCC work.
//!
//! ## Shape
//!
//! ```ignore
//! pub trait Aggregate: Default + Send + Sync + 'static {}
//! pub trait Apply<F: Event>: Aggregate {
//!     fn apply(&mut self, fact: &F);
//! }
//! ```
//!
//! `Aggregate` is a pure marker. The per-Event fold lives in
//! `Apply<F>`. Single-Event aggregates impl `Apply<F>` once; multi-Event
//! aggregates impl `Apply<F1>`, `Apply<F2>`, … for each Event type
//! they fold (one stream per F, selected at `load<A, F>` time).
//!
//! The split lets multiple Aggregates share a Event (e.g. an audit
//! aggregate and a domain aggregate both folding `UserCreated`)
//! without forcing one trait per Event. Mirrors Axon's
//! `AggregateMember` and EventStoreDB's "decider" pattern.

use crate::event::Event;

/// Write-side consistency boundary marker. Per-Event fold behavior
/// lives in [`Apply<F>`]. The aggregate's stream identity comes from
/// the Event's `CATEGORY` const, not from the Aggregate itself —
/// callers always specify both `A` and `F` when loading/appending
/// (`engine.load::<UserAgg, UserCreated>(id)`).
pub trait Aggregate: Default + Send + Sync + 'static {
    /// Stable identifier for this Aggregate type. Used as the key for
    /// in-memory registry state and — critically — as the
    /// `aggregate_type` string a backend writes to disk when
    /// persisting snapshots or events. MUST be stable across
    /// refactorings (renames, module moves) and across builds: pick
    /// it once, treat it like a wire format.
    ///
    /// Two different Aggregates folding the same Event stream MUST
    /// have distinct `NAME`s so their state doesn't collide in the
    /// shared registry. Recommended convention: PascalCase singular
    /// matching the Rust type name (`"UserAgg"`, `"Counter"`,
    /// `"PipelineState"`).
    const NAME: &'static str;
}

/// Per-Event fold for hydration. Called once per fact loaded from the
/// aggregate's stream during `Engine::load<A, F>`. No I/O, no
/// wall-clock — same purity rules as `Projector::project`.
///
/// Borrows `&F` rather than taking ownership: the runner needs the
/// fact again to forward to projectors / reactors, and the fold
/// pattern almost never needs to consume.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not implement `Apply<{F}>` — register an aggregator for `{F}` only after implementing `Apply<{F}> for {Self}`",
    label = "missing `Apply<{F}>` impl",
    note = "every Event that an aggregator folds requires its own `impl Apply<F> for A` block; the engine calls `apply(&mut self, fact: &F)` once per fact"
)]
pub trait Apply<F: Event>: Aggregate {
    fn apply(&mut self, fact: &F);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    enum CounterFact {
        Incremented { by: i32, occurred_at: DateTime<Utc>, counter_id: Uuid },
        Reset { occurred_at: DateTime<Utc>, counter_id: Uuid },
    }

    impl Event for CounterFact {
        const CATEGORY: &'static str = "counter";
        fn event_type(&self) -> &str {
            match self {
                CounterFact::Incremented { .. } => "incremented",
                CounterFact::Reset { .. }       => "reset",
            }
        }
        fn stream_id(&self) -> Uuid {
            match self {
                CounterFact::Incremented { counter_id, .. } => *counter_id,
                CounterFact::Reset { counter_id, .. }       => *counter_id,
            }
        }
        fn occurred_at(&self) -> Option<DateTime<Utc>> {
            Some(match self {
                CounterFact::Incremented { occurred_at, .. } => *occurred_at,
                CounterFact::Reset { occurred_at, .. }       => *occurred_at,
            })
        }
    }

    #[derive(Default, Debug, PartialEq)]
    struct Counter { value: i32 }

    impl Aggregate for Counter {
        const NAME: &'static str = "Counter";
    }
    impl Apply<CounterFact> for Counter {
        fn apply(&mut self, fact: &CounterFact) {
            match fact {
                CounterFact::Incremented { by, .. } => self.value += by,
                CounterFact::Reset { .. }           => self.value = 0,
            }
        }
    }

    #[test]
    fn aggregate_apply_folds_facts_into_state() {
        let cid = Uuid::new_v4();
        let mut agg = Counter::default();
        for fact in [
            CounterFact::Incremented { by: 3, occurred_at: Utc::now(), counter_id: cid },
            CounterFact::Incremented { by: 2, occurred_at: Utc::now(), counter_id: cid },
            CounterFact::Reset       {        occurred_at: Utc::now(), counter_id: cid },
            CounterFact::Incremented { by: 5, occurred_at: Utc::now(), counter_id: cid },
        ] {
            agg.apply(&fact);
        }
        assert_eq!(agg.value, 5, "fold matches manual reduction");
    }

    /// Multi-Event: one aggregate, two Apply impls, each fact type's
    /// CATEGORY identifies its own stream. Folded independently via
    /// `load::<A, F>(id)` per stream.
    #[test]
    fn aggregate_can_apply_multiple_fact_types() {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct Tagged { tag_id: Uuid, occurred_at: DateTime<Utc> }
        impl Event for Tagged {
            const CATEGORY: &'static str = "tag";
            fn event_type(&self) -> &str { "tagged" }
            fn stream_id(&self) -> Uuid { self.tag_id }
            fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        }

        #[derive(Default, Debug)]
        struct Multi { increments: i32, tags: u32 }
        impl Aggregate for Multi {
            const NAME: &'static str = "Multi";
        }
        impl Apply<CounterFact> for Multi {
            fn apply(&mut self, f: &CounterFact) {
                if let CounterFact::Incremented { by, .. } = f { self.increments += by; }
            }
        }
        impl Apply<Tagged> for Multi {
            fn apply(&mut self, _f: &Tagged) { self.tags += 1; }
        }

        let cid = Uuid::new_v4();
        let mut m = Multi::default();
        Apply::<CounterFact>::apply(&mut m, &CounterFact::Incremented {
            by: 7, occurred_at: Utc::now(), counter_id: cid,
        });
        Apply::<Tagged>::apply(&mut m, &Tagged {
            tag_id: cid, occurred_at: Utc::now(),
        });
        assert_eq!(m.increments, 7);
        assert_eq!(m.tags, 1);
    }
}
