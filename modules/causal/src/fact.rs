//! `Fact` trait — v0.4 application-facing surface for events.
//!
//! Every value appended to the event log implements `Fact`. The trait
//! is Kurrent-aligned by design:
//!
//! - [`CATEGORY`](Fact::CATEGORY): per-type const, the prefix of
//!   Kurrent's `category-id` stream-name convention. Every variant
//!   of a Fact enum shares one category. Subscriptions are by
//!   category (`$ce-{CATEGORY}`).
//! - [`name`](Fact::name): per-variant event name. The runtime
//!   composes Kurrent's `event_type` as `format!("{}:{}",
//!   Self::CATEGORY, self.name())`.
//! - [`stream_id`](Fact::stream_id): per-variant stream id. The
//!   runtime composes the stream name as `format!("{}-{}",
//!   Self::CATEGORY, self.stream_id())`.
//! - [`occurred_at`](Fact::occurred_at): optional producer-claimed
//!   occurrence time. Defaults to `None`; runners fall back to
//!   `event.created_at` (the persistence-side envelope timestamp).
//!
//! Phase 6 macros (`#[fact(category = "..."])`) generate the impl
//! from the enum definition. Hand-rolling is also fine — single
//! const, two methods.

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

/// Application-facing event type. Implemented per fact value.
pub trait Fact: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Stream category — the prefix portion of Kurrent's
    /// `category-id` stream name convention. Identifies the kind of
    /// entity this Fact's variants are about. All variants share
    /// one category.
    ///
    /// Subscriptions filter by this via `$ce-{CATEGORY}` system
    /// streams in Kurrent; backends with no native category-stream
    /// support filter by `aggregate_type = CATEGORY`.
    const CATEGORY: &'static str;

    /// Per-variant event name. Stored as Kurrent's `event_type`
    /// after the runtime composes `format!("{}:{}", Self::CATEGORY,
    /// self.name())`. Bare variant slug — do NOT include the
    /// category prefix here; the runtime adds it.
    fn name(&self) -> &str;

    /// Per-variant stream id. Combined with [`Self::CATEGORY`] the
    /// runtime composes the Kurrent-style stream name
    /// `format!("{}-{}", Self::CATEGORY, self.stream_id())`.
    fn stream_id(&self) -> Uuid;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct ScheduleCreated {
        schedule_id: Uuid,
        occurred_at: DateTime<Utc>,
    }

    impl Fact for ScheduleCreated {
        const CATEGORY: &'static str = "schedule";
        fn name(&self) -> &str { "created" }
        fn stream_id(&self) -> Uuid { self.schedule_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    #[test]
    fn fact_round_trips_via_serde() {
        let f = ScheduleCreated {
            schedule_id: Uuid::nil(),
            occurred_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: ScheduleCreated = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn category_is_per_type_const() {
        assert_eq!(<ScheduleCreated as Fact>::CATEGORY, "schedule");
    }

    #[test]
    fn name_is_bare_variant_slug() {
        // No "category:" prefix — the runtime composes that.
        let f = ScheduleCreated {
            schedule_id: Uuid::nil(),
            occurred_at: Utc::now(),
        };
        assert_eq!(f.name(), "created");
    }

    #[test]
    fn stream_id_is_per_variant() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let f = ScheduleCreated { schedule_id: id, occurred_at: Utc::now() };
        assert_eq!(f.stream_id(), id);
    }

    #[test]
    fn runtime_stream_name_composes_category_dash_stream_id() {
        // The Kurrent-style stream name is built from CATEGORY +
        // stream_id with `-` separator. Runtime does this; the
        // trait surface narrows to providing the components.
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let f = ScheduleCreated { schedule_id: id, occurred_at: Utc::now() };
        let stream_name = format!("{}-{}", <ScheduleCreated as Fact>::CATEGORY, f.stream_id());
        assert_eq!(stream_name, format!("schedule-{}", id));
    }

    #[test]
    fn runtime_event_type_composes_category_colon_name() {
        // The Kurrent-style event_type is built from CATEGORY +
        // name with `:` separator. Runtime does this on emit.
        let f = ScheduleCreated {
            schedule_id: Uuid::nil(),
            occurred_at: Utc::now(),
        };
        let event_type = format!("{}:{}", <ScheduleCreated as Fact>::CATEGORY, f.name());
        assert_eq!(event_type, "schedule:created");
    }
}
