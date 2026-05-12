//! v0.3 `Aggregate` trait — write-side consistency boundary.
//!
//! Lives at `crate::aggregate_v3::Aggregate` until Phase 9 collapses
//! the legacy `crate::aggregator::Aggregate` (which uses per-event
//! `Apply<E>` impls and an `AggregatorRegistry`). The v0.3 form is
//! single-Fact-per-aggregate with apply on the trait itself, mirroring
//! the rest of the v0.4 trait taxonomy (Projector / Reactor).
//!
//! Aggregates are the consistency boundary on the write path — the
//! place where command-time invariants are enforced before facts hit
//! the log. Per C6, `Engine::append<A>` is OCC-protected: command
//! handlers `load<A>` to fold the stream, decide, then `append<A>`
//! with `expected_version`; the backend errors with `Conflict` if the
//! stream moved.
//!
//! Per C11, Reactors cannot emit into Aggregate streams; saga-shaped
//! "emit only if aggregate at version V" operations must be modeled
//! as command handlers. Phase 5a's `EngineBuilder::with_aggregate`
//! registers the aggregate's stream as `StreamPolicy::OccRequired`
//! so `Engine::emit` rejects writes to it; Phase 5b wires up
//! `load<A>` / `append<A>` to actually do the OCC work.

use crate::fact::Fact;

/// Write-side consistency boundary. Aggregates are folded from a
/// dedicated stream named `format!("{CATEGORY}-{id}")`; command
/// handlers `engine.load::<A>(id)` to hydrate, decide, then
/// `engine.append::<A>(id, expected_version, new_facts)` with OCC.
pub trait Aggregate: Default + Send + Sync + 'static {
    type Fact: Fact;

    /// Per-type stream category. Used to compute stream names
    /// (`format!("{CATEGORY}-{id}")`) and to register the stream
    /// policy as OCC-required at builder time. Typically the singular
    /// aggregate noun: `"schedule"`, `"order"`, `"region"`.
    const CATEGORY: &'static str;

    /// Pure fold for hydration. Called once per fact loaded from the
    /// aggregate's stream during `Engine::load<A>`. No I/O, no
    /// wall-clock — same purity rules as `View::apply`.
    fn apply(&mut self, fact: &Self::Fact);
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

    impl Fact for CounterFact {
        const CATEGORY: &'static str = "counter";
        fn name(&self) -> &str {
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
        type Fact = CounterFact;
        const CATEGORY: &'static str = "counter";

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

    #[test]
    fn aggregate_category_drives_stream_name() {
        let cid = Uuid::new_v4();
        let _fact = CounterFact::Incremented {
            by: 1, occurred_at: Utc::now(), counter_id: cid,
        };
        // Under v0.4, the Fact's CATEGORY const is the type-level
        // constant; aggregate Fact::CATEGORY matches it by trait.
        assert_eq!(<CounterFact as Fact>::CATEGORY, <Counter as Aggregate>::CATEGORY);
    }
}
