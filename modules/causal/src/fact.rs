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
///
/// # v0.4 migration note
///
/// The trait is in the middle of an additive migration from v0.3
/// shape (`type_name` + `type_prefix` + `stream` returning
/// `StreamRef`) to v0.4 shape (`CATEGORY` const + `name()` +
/// `stream_id()`). During P1-P10, both shapes coexist:
///
/// - v0.3 methods (`type_name`, `type_prefix`, `stream`) remain
///   required for backwards compatibility with existing impls.
/// - v0.4 methods (`name`, `category`, `stream_id`) have default
///   impls that delegate to v0.3 methods. New code uses v0.4
///   methods.
///
/// P11 removes the v0.3 methods entirely and converts `category()`
/// into the `CATEGORY` associated const (the locked target shape
/// from the v0.4 design plan). This requires every Fact impl to
/// update at that time.
pub trait Fact: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// **v0.3 method, will be removed in v0.4 P11.**
    ///
    /// Per-variant routing name, e.g. `"scheduling.schedule_created"`.
    /// In v0.4 this is replaced by `name()` (variant-only, no
    /// namespace prefix) — the runtime composes `event_type` from
    /// `format!("{}:{}", Self::category(), self.name())`.
    fn type_name(&self) -> &str;

    /// **v0.3 method, will be removed in v0.4 P11.**
    ///
    /// Per-type const prefix, e.g. `"scheduling"`. In v0.4 this is
    /// replaced by the `category()` const-style method (renamed for
    /// Kurrent alignment).
    fn type_prefix() -> &'static str;

    // ── v0.4 surface (additive; defaults delegate to v0.3) ────────

    /// **v0.4.** Stream category — the prefix portion of Kurrent's
    /// `category-id` stream name convention. Replaces v0.3's
    /// `type_prefix()`. In P11 this becomes `const CATEGORY:
    /// &'static str` per the locked design.
    ///
    /// Default impl delegates to `type_prefix()` for compatibility
    /// with existing v0.3 Fact impls during the migration window.
    fn category() -> &'static str where Self: Sized {
        Self::type_prefix()
    }

    /// **v0.4.** Per-variant event name. Stored as Kurrent
    /// `event_type` via `format!("{}:{}", Self::category(),
    /// self.name())`. Replaces v0.3's `type_name()`.
    ///
    /// Default impl delegates to `type_name()` for compatibility.
    /// Note: existing v0.3 impls typically return
    /// `"prefix.variant"`; v0.4 impls should return just `"variant"`
    /// since the runtime composes the prefix. Migration-time
    /// stripping happens in the P0 runbook.
    fn name(&self) -> &str {
        self.type_name()
    }

    /// **v0.4.** Per-variant stream id. Replaces v0.3's `stream()`
    /// returning `StreamRef { category, id }`. Under v0.4 the
    /// runtime composes the stream name from `Self::category()`
    /// (per-type) + `self.stream_id()` (per-instance).
    ///
    /// Default impl delegates to `self.stream().id`. In P11 this
    /// becomes the only stream-identification method; `stream()`
    /// returning `StreamRef` is removed.
    fn stream_id(&self) -> Uuid {
        self.stream().id
    }

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

    // ── v0.4 surface tests (additive — delegates to v0.3 by default) ──

    #[test]
    fn fact_category_defaults_to_type_prefix() {
        // The default impl of `category()` delegates to
        // `type_prefix()`. New v0.4 callers can use either form
        // during the additive-migration window; they must produce
        // the same string.
        assert_eq!(<Pinged as Fact>::category(), "test");
        assert_eq!(<Pinged as Fact>::category(), <Pinged as Fact>::type_prefix());
    }

    #[test]
    fn fact_name_defaults_to_type_name() {
        // Per-variant `name()` delegates to `type_name()` for
        // v0.3 impls. New v0.4 impls should return just the
        // variant slug (no namespace prefix); the runtime
        // composes `event_type` as `format!("{}:{}", category,
        // name)`.
        let f = Pinged {
            schedule_id: Uuid::nil(),
            occurred_at: Utc::now(),
        };
        assert_eq!(f.name(), f.type_name());
    }

    #[test]
    fn fact_stream_id_defaults_to_stream_dot_id() {
        // v0.4 `stream_id()` defaults to `self.stream().id`. The
        // stream NAME composition (category + id) moves to the
        // runtime; the trait surface narrows to just returning
        // the per-variant id.
        let schedule_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let f = Pinged { schedule_id, occurred_at: Utc::now() };
        assert_eq!(f.stream_id(), f.stream().id);
        assert_eq!(f.stream_id(), schedule_id);
    }

    /// v0.4-aligned fixture: prefix == category. Under v0.4's
    /// single-axis model, every Fact enum must satisfy this — one
    /// CATEGORY per Fact, no divergence between subscription axis
    /// and storage axis.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct AlignedFact {
        order_id: Uuid,
    }

    impl Fact for AlignedFact {
        fn type_name(&self) -> &str { "order.placed" }
        fn type_prefix() -> &'static str { "order" }
        fn stream(&self) -> StreamRef {
            StreamRef { category: "order", id: self.order_id }
        }
    }

    #[test]
    fn v04_stream_name_composes_from_category_and_stream_id_for_aligned_facts() {
        // For Facts where v0.3 type_prefix matches v0.3
        // stream().category — the v0.4-clean shape — the two
        // composition paths produce the same Kurrent stream name.
        let order_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let f = AlignedFact { order_id };

        let v04_stream_name = format!("{}-{}", <AlignedFact as Fact>::category(), f.stream_id());
        let v03_stream_name = f.stream().name();
        assert_eq!(v04_stream_name, v03_stream_name);
        assert_eq!(v04_stream_name, format!("order-{}", order_id));
    }

    #[test]
    fn v04_composition_diverges_for_misaligned_v03_facts() {
        // For Facts where v0.3 has prefix != stream().category (the
        // divergent case the v0.4 design forbids), the two
        // composition paths produce DIFFERENT stream names. This
        // test pins the property — and documents the migration
        // hazard: backends using the v0.4 composition during the
        // additive-migration window would write to the wrong stream
        // for divergent Facts.
        //
        // Mitigation (per the P0 runbook): consumers MUST refactor
        // divergent Facts to align before P11 deletes the v0.3
        // methods. SchedulingEvent (prefix="scheduling", category
        // varying between "schedule" and "scrape_schedule") is the
        // canonical example.
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let f = Pinged { schedule_id: id, occurred_at: Utc::now() };

        let v04_stream_name = format!("{}-{}", <Pinged as Fact>::category(), f.stream_id());
        let v03_stream_name = f.stream().name();
        assert_ne!(
            v04_stream_name, v03_stream_name,
            "Pinged fixture is intentionally divergent (prefix=test, category=schedule); \
             the two composition paths MUST produce different names — proves the migration \
             hazard the P0 runbook addresses."
        );
        assert_eq!(v04_stream_name, format!("test-{}", id));
        assert_eq!(v03_stream_name, format!("schedule-{}", id));
    }
}
