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
///
/// `:` is the format separator and MUST NOT appear in `category` — a
/// colon there desyncs the two matchers ([`matches_category`] is
/// length-based; [`category_of`] / the aggregator's `extract_prefix`
/// split on the FIRST colon), so a reactor would fire while the
/// aggregate silently never folds. [`validate_category`] catches this
/// loudly at engine build; this `debug_assert` is the developer-time
/// backstop on the emit path.
pub fn compose(category: &str, name: &str) -> String {
    debug_assert!(
        !category.contains(':'),
        "Event::CATEGORY `{category}` contains the reserved separator ':' — \
         categories must be colon-free (see causal::event_type::validate_category)",
    );
    format!("{category}:{name}")
}

/// Reject a category that can't participate in the `{category}:{name}`
/// format: empty (acts as a `:`-prefix wildcard) or containing the
/// reserved `:` separator (silently desyncs reactor matching from
/// aggregate folding). Called at engine build for every registered
/// aggregator category + stream category, so the failure is loud and
/// early rather than a silent never-folds at runtime.
pub fn validate_category(kind: &str, category: &str) -> anyhow::Result<()> {
    if category.is_empty() {
        anyhow::bail!("{kind} category is empty — categories must be non-empty");
    }
    if category.contains(':') {
        anyhow::bail!(
            "{kind} category `{category}` contains the reserved separator ':' — \
             a colon there makes reactors fire while the aggregate silently \
             never folds; rename the category to be colon-free"
        );
    }
    Ok(())
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
    // An empty category would make `event_type.as_bytes()[0] == b':'`
    // a `:`-prefix wildcard; a valid category is never empty
    // (validate_category enforces this at build), but guard here too.
    !category.is_empty()
        && event_type.len() > category.len()
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

    #[test]
    fn empty_category_is_not_a_wildcard() {
        // Regression (adversarial-input fuzzer): an empty category once
        // matched any `:`-prefixed event_type because `bytes[0] == b':'`.
        assert!(!matches_category(":anything", ""));
        assert!(!matches_category("order:placed", ""));
    }

    #[test]
    fn validate_category_rejects_colon_and_empty() {
        assert!(validate_category("x", "order").is_ok());
        assert!(validate_category("x", "scout_run").is_ok());

        let colon = validate_category("x", "order:sub").unwrap_err().to_string();
        assert!(colon.contains("reserved separator"), "got: {colon}");

        let empty = validate_category("x", "").unwrap_err().to_string();
        assert!(empty.contains("empty"), "got: {empty}");
    }
}
