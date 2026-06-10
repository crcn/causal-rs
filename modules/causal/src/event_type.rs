//! Single owner of the composed `{category}:{name}` event-type format.
//!
//! The stored `event_type` namespaces the per-variant name with the
//! ROUTING category (`Event::CATEGORY`) so two categories can both
//! have an `OrderPlaced` without colliding in `$et-` streams or typed
//! dispatch. This is a **deliberate divergence** from KurrentDB
//! convention (plain event-type names) — see the README's divergence
//! list.
//!
//! Every compose / parse / match of the format lives here. Before this
//! module (2026-06-10 audit remediation, B1) the format was scattered
//! across five sites, two of which matched with a bare
//! `starts_with(category)` — so category `"order"` matched
//! `"orders:created"` and a foreign payload reached the trigger
//! deserializer.

/// Compose the stored event_type: `{category}:{name}`.
pub fn compose(category: &str, name: &str) -> String {
    format!("{category}:{name}")
}

/// The routing category of a composed event_type — everything before
/// the first `:` (the whole string if there is none, for foreign /
/// legacy events).
pub fn category_of(event_type: &str) -> &str {
    event_type.split(':').next().unwrap_or(event_type)
}

/// True iff `event_type` belongs to `category` — i.e. it matches
/// `format!("{category}:*")`. Colon-aware: avoids the false positive a
/// bare `starts_with` has when one category is a prefix of another
/// (`"order"` must not match `"orders:created"`).
pub fn matches_category(event_type: &str, category: &str) -> bool {
    event_type.len() > category.len()
        && event_type.as_bytes()[category.len()] == b':'
        && event_type.starts_with(category)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_and_parse_roundtrip() {
        let t = compose("order", "placed");
        assert_eq!(t, "order:placed");
        assert_eq!(category_of(&t), "order");
    }

    #[test]
    fn category_of_foreign_event_is_whole_string() {
        assert_eq!(category_of("SomeForeignEvent"), "SomeForeignEvent");
    }

    #[test]
    fn matches_category_is_colon_aware() {
        assert!(matches_category("order:placed", "order"));
        assert!(!matches_category("orders:created", "order"),
                "prefix category must not match a longer category");
        assert!(!matches_category("order_audit:logged", "order"));
        assert!(!matches_category("order", "order"),
                "bare category with no name is not a composed type");
        assert!(!matches_category("ord:x", "order"));
    }
}
