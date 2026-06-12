//! `Event` trait — application-facing surface for events.
//!
//! Every value appended to the event log implements `Event`. The trait
//! is Kurrent-aligned by design:
//!
//! - [`CATEGORY`](Event::CATEGORY): per-type const, the prefix of
//!   Kurrent's `category-id` stream-name convention. Every variant
//!   of a Event enum shares one category. Subscriptions are by
//!   category (`$ce-{CATEGORY}`).
//! - [`event_type`](Event::event_type): per-variant event type name.
//!   The runtime composes Kurrent's stored `event_type` as
//!   `format!("{}:{}", Self::CATEGORY, self.event_type())`.
//! - [`subject_id`](Event::subject_id): per-variant stream id. The
//!   runtime composes the stream name as `format!("{}-{}",
//!   Self::CATEGORY, self.subject_id())`.
//! - [`occurred_at`](Event::occurred_at): optional producer-claimed
//!   occurrence time. Defaults to `None`; runners fall back to
//!   `event.created_at` (the persistence-side envelope timestamp).
//!
//! The `#[event(prefix = "...")]` macro (re-exported as
//! [`causal::event`]) generates the impl from an enum definition.
//! Hand-rolling is also fine — single const, two methods.

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

/// Application-facing event type. Implemented per fact value.
pub trait Event: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Stream category — the prefix portion of Kurrent's
    /// `category-id` stream name convention. Identifies the kind of
    /// entity this Event's variants are about. All variants share
    /// one category.
    ///
    /// Subscriptions filter by this via `$ce-{CATEGORY}` system
    /// streams in Kurrent; backends with no native category-stream
    /// support filter by the `category` field (= `CATEGORY`).
    const CATEGORY: &'static str;

    /// Stream the event is *stored* in — `{SUBJECT}-{subject_id}`.
    /// Defaults to [`CATEGORY`](Self::CATEGORY), so by default an event
    /// streams by its own category (the original behavior).
    ///
    /// Override to co-locate several distinct event types in ONE stream
    /// (e.g. every event a single aggregate folds) while each type keeps
    /// its own `CATEGORY` for consumer/aggregator routing. Routing always
    /// uses `event_type` (`{CATEGORY}:{name}`); only physical stream
    /// placement uses `SUBJECT`. An aggregate that wants durable
    /// restore reads from this stream via `Aggregate::SUBJECT`.
    const SUBJECT: &'static str = Self::CATEGORY;

    /// Per-variant event type name. The runtime composes the
    /// stored `event_type` field as `format!("{}:{}", Self::CATEGORY,
    /// self.event_type())`. Return the bare variant slug here — do
    /// NOT include the category prefix; the runtime adds it.
    fn event_type(&self) -> &str;

    /// Per-variant stream id. Combined with [`Self::CATEGORY`] the
    /// runtime composes the Kurrent-style stream name
    /// `format!("{}-{}", Self::CATEGORY, self.subject_id())`.
    fn subject_id(&self) -> Uuid;

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

/// Compose the canonical Kurrent-style stream name for a fact:
/// `{F::SUBJECT}-{subject_id}`.
///
/// Returns the same string the runtime actually appends to — the
/// *physical placement* stream, `SUBJECT` (defaults to
/// `CATEGORY`, but distinct for co-located event types). Useful when you
/// need the stream name out-of-band — e.g., constructing a Kurrent
/// admin-UI link, running a raw `read_stream` against the underlying
/// client, or asserting a stream name in tests.
pub fn stream_name_for<F: Event>(id: uuid::Uuid) -> String {
    format!("{}-{}", F::SUBJECT, id)
}

/// Compose the canonical event_type for a fact:
/// `{F::CATEGORY}:{fact.event_type()}`. Same shape `Engine::emit` writes
/// to `RecordedEvent::event_type` and Kurrent's `event_type` field.
pub fn event_type_for<F: Event>(fact: &F) -> String {
    crate::event_type::compose(F::CATEGORY, fact.event_type())
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

    impl Event for ScheduleCreated {
        const CATEGORY: &'static str = "schedule";
        fn event_type(&self) -> &str { "created" }
        fn subject_id(&self) -> Uuid { self.schedule_id }
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
        assert_eq!(<ScheduleCreated as Event>::CATEGORY, "schedule");
    }

    #[test]
    fn name_is_bare_variant_slug() {
        // No "category:" prefix — the runtime composes that.
        let f = ScheduleCreated {
            schedule_id: Uuid::nil(),
            occurred_at: Utc::now(),
        };
        assert_eq!(f.event_type(), "created");
    }

    #[test]
    fn subject_id_is_per_variant() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let f = ScheduleCreated { schedule_id: id, occurred_at: Utc::now() };
        assert_eq!(f.subject_id(), id);
    }

    #[test]
    fn runtime_stream_name_composes_category_dash_subject_id() {
        // The Kurrent-style stream name is built from CATEGORY +
        // subject_id with `-` separator. Runtime does this; the
        // trait surface narrows to providing the components.
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let f = ScheduleCreated { schedule_id: id, occurred_at: Utc::now() };
        let stream_name = format!("{}-{}", <ScheduleCreated as Event>::CATEGORY, f.subject_id());
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
        let event_type = format!("{}:{}", <ScheduleCreated as Event>::CATEGORY, f.event_type());
        assert_eq!(event_type, "schedule:created");
    }
}
