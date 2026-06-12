//! `Event` trait — application-facing surface for events.
//!
//! Every value appended to the event log implements `Event`. Three
//! identity claims, all declared, machinery derived (no-lying-defaults
//! Primitive 1):
//!
//! - [`NAME`](Event::NAME): the fact's kind — the exact `event_type`
//!   on the wire. Routing matches it by EQUALITY (no prefixes, no
//!   composition): vocabulary growth is additive because a new kind
//!   matches no existing consumer.
//! - [`SUBJECT`](Event::SUBJECT)/[`subject_id`](Event::subject_id):
//!   which thing the fact is about. The runtime derives the storage
//!   stream as `{SUBJECT}-{subject_id}`.
//! - [`occurred_at`](Event::occurred_at): optional producer-claimed
//!   occurrence time. Defaults to `None`; runners fall back to
//!   `event.created_at` (the persistence-side envelope timestamp).
//!
//! The `#[event(name = "...")]` macro (re-exported as
//! [`causal::event`]) generates the impl. Hand-rolling is also fine —
//! one const, one method.

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

/// Application-facing event type. Implemented per fact value.
pub trait Event: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// The fact's kind — written verbatim to the envelope's
    /// `event_type` and matched by consumers by EQUALITY. A wire
    /// format: pick it once, treat it like `Aggregate::NAME`.
    /// Renaming it orphans historical events from new consumers —
    /// that's what the `vocab_check!` CI gate exists to catch.
    const NAME: &'static str;

    /// The subject KIND whose history this fact joins — the storage
    /// stream is `{SUBJECT}-{subject_id}`. Defaults to
    /// [`NAME`](Self::NAME): a fact streams by its own kind unless it
    /// co-locates with other fact families in one subject history
    /// (set `subject = "<kind>"` in the macro / override here).
    const SUBJECT: &'static str = Self::NAME;

    /// Which subject this fact is about. With [`Self::SUBJECT`] the
    /// runtime derives the storage stream name
    /// `format!("{}-{}", Self::SUBJECT, self.subject_id())`.
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
    /// Consumers see the resolved value as `ctx.time()`. Replay
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
    /// emit-time and replay-time `ctx.time()` silently disagree — and
    /// replay determinism is the whole point of the trait.
    fn occurred_at(&self) -> Option<DateTime<Utc>> { None }

    /// Which workflow this fact roots, named BY its own payload field —
    /// `Some` makes the fact a **workflow ROOT** (macro
    /// `workflow_id = "field"`); `None` (the default) makes it a chain
    /// member that inherits its emitter's workflow.
    ///
    /// Constitutional, not per-call-site: a fact type is a root or a
    /// member at every emit site, from any emitter. The envelope
    /// `workflow_id` is stamped FROM this value, so payload and
    /// envelope cannot disagree; `.workflow_id(x)` on the emit builder
    /// against a declaring type is a loud error, never a silent
    /// precedence.
    ///
    /// The field's value must be derived (`ctx.derive_id`) — a
    /// `new_v4()` workflow id mints a phantom workflow on redelivery.
    /// The field may also carry an EXISTING workflow's id (the
    /// named-mailbox pattern: the fact joins that live workflow's
    /// ordering and settle scope) — the derivation at the emit site is
    /// what reviewers review.
    fn declared_workflow_id(&self) -> Option<Uuid> { None }
}

/// Compose the canonical stream name for a fact's subject history:
/// `{F::SUBJECT}-{subject_id}` — the *physical placement* stream the
/// runtime actually appends to. Useful out-of-band: Kurrent admin-UI
/// links, raw `read_stream` calls, stream-name assertions in tests.
pub fn stream_name_for<F: Event>(id: uuid::Uuid) -> String {
    format!("{}-{}", F::SUBJECT, id)
}

/// The canonical `event_type` for a fact — exactly [`Event::NAME`].
/// Kept as a helper for symmetry with [`stream_name_for`]; there is no
/// composition anymore.
pub fn event_type_for<F: Event>(_fact: &F) -> String {
    F::NAME.to_string()
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
        const NAME: &'static str = "schedule_created";
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
    fn name_is_the_wire_event_type() {
        assert_eq!(<ScheduleCreated as Event>::NAME, "schedule_created");
        let f = ScheduleCreated { schedule_id: Uuid::nil(), occurred_at: Utc::now() };
        assert_eq!(event_type_for(&f), "schedule_created");
    }

    #[test]
    fn subject_defaults_to_name() {
        assert_eq!(<ScheduleCreated as Event>::SUBJECT, "schedule_created");
    }

    #[test]
    fn stream_name_composes_subject_dash_id() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        assert_eq!(
            stream_name_for::<ScheduleCreated>(id),
            format!("schedule_created-{id}"),
        );
    }
}
