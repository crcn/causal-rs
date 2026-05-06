//! `Fact` trait — v0.3 application-facing surface for events.
//!
//! Every value appended to the event log implements `Fact`. The trait
//! mirrors causal 0.2.2's `Event` trait (per-variant `type_name` +
//! per-type `type_prefix`) and adds two value-level methods that v0.3
//! requires:
//!
//! - `occurred_at()`: optional producer-claimed occurrence time.
//!   Defaults to `None`; runners fall back to `event.created_at`
//!   (the persistence-side envelope timestamp). Override only for
//!   domains with backdating, batch historical import, or out-of-
//!   order arrival from external producers (Stripe webhooks, Slack
//!   delivery delay, mobile offline buffer). Consumers reach this
//!   via `ctx.now()` — replay reproduces byte-identical state
//!   because no wall-clock accessor is exposed.
//! - `stream()`: declares the named stream this fact belongs to.
//!   Mandatory because Kurrent has no stream-less events.
//!
//! There is no blanket impl from `Event` because `occurred_at` and
//! `stream` are per-type and cannot be derived from `Event` alone.
//! Phase 6 macros will generate `Fact` impls from `#[event(stream =
//! "category.{field}")]` annotations; until then, hand-roll impls.

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

/// Reference to the named stream a fact belongs to.
///
/// Stream name is `format!("{category}-{id}")`, matching Kurrent's
/// `category-id` convention so `$ce-{category}` subscriptions work
/// without further translation.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct StreamRef {
    pub category: &'static str,
    pub id:       Uuid,
}

impl StreamRef {
    /// Canonical stream name for backend persistence.
    pub fn name(&self) -> String {
        format!("{}-{}", self.category, self.id)
    }
}

/// Application-facing event type. Implemented per fact value.
pub trait Fact: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Per-variant routing name, e.g. `"scheduling.schedule_created"`.
    /// Used by backends for `$et-{type}` subscriptions and by
    /// projection filters.
    fn type_name(&self) -> &str;

    /// Per-type const prefix, e.g. `"scheduling"`. Mirrors the existing
    /// `Event::event_prefix` so `#[event(prefix = "...")]` enums
    /// continue to work; Phase 6 macros will emit both Event and Fact
    /// impls for the same enum.
    fn type_prefix() -> &'static str;

    /// Optional producer-claimed occurrence time. Defaults to `None` —
    /// runners fall back to `event.created_at` (the persistence-side
    /// envelope timestamp).
    ///
    /// Override only for domains with backdating, batch historical
    /// import, or out-of-order arrival from external producers
    /// (Stripe webhooks, Slack delivery delay, mobile offline buffer).
    /// For domains where logical time IS log time, leave this at the
    /// default — no carrier-event payload bloat for a value the
    /// consumer would never read differently from `event.created_at`.
    ///
    /// Consumers see the resolved value as `ctx.now()`. Replay
    /// reproduces byte-identical state because no wall-clock accessor
    /// is exposed inside apply bodies.
    ///
    /// # Invariant — deterministic from the serialized form
    ///
    /// If you override this, the returned value MUST be reproducible
    /// from the fact's serde representation:
    ///
    /// ```text
    /// for any fact F:
    ///   F.occurred_at() ==
    ///     serde_json::from_value::<Self>(serde_json::to_value(&F)?)?
    ///       .occurred_at()
    /// ```
    ///
    /// In practice: read from a serde-visible field. NEVER read from
    /// a `#[serde(skip)]` field, a computed-on-construction value, or
    /// a non-deterministic source (`Utc::now()`). Violating this makes
    /// emit-time and replay-time `ctx.now()` silently disagree — and
    /// replay determinism is the whole point of the trait.
    fn occurred_at(&self) -> Option<DateTime<Utc>> { None }

    /// The stream this fact belongs to. Mandatory — no stream-less
    /// emission. Aggregate streams are typically `(A::CATEGORY, agg_id)`;
    /// observation streams use a category like `"observations"` plus
    /// a correlation id.
    fn stream(&self) -> StreamRef;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Pinged {
        schedule_id: Uuid,
        occurred_at: DateTime<Utc>,
    }

    impl Fact for Pinged {
        fn type_name(&self) -> &str { "test.pinged" }
        fn type_prefix() -> &'static str { "test" }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
        fn stream(&self) -> StreamRef {
            StreamRef { category: "schedule", id: self.schedule_id }
        }
    }

    #[test]
    fn fact_round_trips_via_serde() {
        let f = Pinged {
            schedule_id: Uuid::nil(),
            occurred_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: Pinged = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn stream_ref_name_uses_kurrent_convention() {
        let schedule_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let f = Pinged {
            schedule_id,
            occurred_at: Utc::now(),
        };
        assert_eq!(
            f.stream().name(),
            format!("schedule-{}", schedule_id),
        );
    }

    #[test]
    fn fact_type_name_distinguishes_per_instance() {
        let f = Pinged {
            schedule_id: Uuid::nil(),
            occurred_at: Utc::now(),
        };
        assert_eq!(f.type_name(), "test.pinged");
    }

    #[test]
    fn fact_type_prefix_is_per_type_const() {
        assert_eq!(<Pinged as Fact>::type_prefix(), "test");
    }
}
