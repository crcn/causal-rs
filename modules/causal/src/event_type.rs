//! Single owner of event-kind validation and matching.
//!
//! Since the 0.10 flat-routing change (step-1 chunk 7c), the stored
//! `event_type` IS [`Event::NAME`](crate::event::Event::NAME) —
//! verbatim, no `{category}:{name}` composition, no prefixes. Routing
//! matches by **equality**, which kills the prefix-collision bug class
//! outright (pre-B1, a bare `starts_with` let category `"order"` match
//! `"orders:created"`) and makes vocabulary growth additive: a new
//! kind matches no existing consumer.
//!
//! This module retains the two operations that survive the format:
//! name validation at registration/build time, and the (now trivial,
//! but single-owner) match predicate.

/// Reject an event kind / subject that can't serve as a wire identity:
/// empty. Called at engine build for every registered aggregator kind
/// and subject, so the failure is loud and early.
///
/// No other charset restrictions: with equality matching there is no
/// separator to protect. (The Kurrent backend still rejects `-` in
/// SUBJECT values, per its `{subject}-{uuid}` stream-name convention —
/// that check lives at the storage boundary where it's true.)
pub fn validate_name(what: &str, name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!(
            "{what} is empty — an event kind / subject is a wire identity \
             and must be a non-empty string",
        );
    }
    Ok(())
}

/// Does a stored `event_type` belong to kind `name`? Exact equality —
/// kept as a named, single-owner predicate so the matching rule has
/// one home (and one place to instrument) rather than `==` scattered
/// across the runners.
#[inline]
pub fn matches_kind(event_type: &str, name: &str) -> bool {
    event_type == name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_exact_no_prefixes() {
        assert!(matches_kind("order_placed", "order_placed"));
        // The pre-B1 bug class is unconstructable under equality.
        assert!(!matches_kind("orders_created", "order"));
        assert!(!matches_kind("order_placed", "order"));
        assert!(!matches_kind("order", "order_placed"));
    }

    #[test]
    fn colons_are_just_characters_now() {
        // Historical composed names remain valid opaque kinds.
        assert!(matches_kind("legacy:created", "legacy:created"));
        assert!(!matches_kind("legacy:created", "legacy"));
    }

    #[test]
    fn empty_names_are_rejected() {
        assert!(validate_name("aggregator event", "").is_err());
        assert!(validate_name("x", "order_placed").is_ok());
    }
}
