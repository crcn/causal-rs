//! The retry taxonomy (BLOCKING-2) — error classes, declared by the
//! reactor, that decide retry policy. No blanket policy in either
//! direction: blanket bounded-retry turns a ten-minute infra outage
//! into a mass manual-replay incident; blanket infinite-retry turns a
//! poison payload into a permanently wedged partition.
//!
//! | Class | Examples | Policy |
//! |---|---|---|
//! | [`transient`] | connection refused, 5xx, timeout, 429 | capped backoff up to a **liveness-time ceiling** (hours, not attempts), then parks as `transient_exhausted` |
//! | [`poison`] | deterministic failures — bad payload shapes, config errors | parks immediately; retry is pointless |
//! | [`domain`] | the operation itself failed meaningfully | bounded attempts, then parks as `domain` |
//! | *(unclassified)* | any plain `anyhow::Error` | domain policy, parked as `unclassified` — labeled honestly, not masqueraded as a declared class |
//!
//! ```ignore
//! async fn react(&self, t: &JobOpened, ctx: Ctx<'_>) -> Result<Events> {
//!     let page = http.fetch(&t.url).await.map_err(causal::transient)?;
//!     let parsed = parse(&page).map_err(causal::poison)?;       // determinate
//!     let quote = pricing.quote(&parsed).map_err(causal::domain)?;
//!     Ok(causal::events![QuoteReady { job_id: t.job_id, quote }])
//! }
//! ```
//!
//! The transient ceiling is measured in **liveness time** (tokio
//! `Instant` — virtualizable under `start_paused`), never as a chrono
//! comparison: a wall-clock ceiling would make exactly this machinery
//! untestable outside production (Primitive 6's two-clock rule). It is
//! ceilinged, not unbounded, because misclassification is inevitable
//! (an LLM 400 for an oversized prompt is deterministic but looks
//! transient from a client adapter) — an unbounded-transient partition
//! would be a silent permanent wedge.

use std::fmt;

/// Declared classification of a reactor error — attach with
/// [`transient`] / [`poison`] / [`domain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Infrastructure blip: retry with capped backoff up to the
    /// liveness-time ceiling, then park as `transient_exhausted`.
    Transient,
    /// Deterministic failure: park immediately; retrying cannot help.
    Poison,
    /// The operation failed meaningfully: bounded attempts, then park.
    Domain,
}

/// What the failure store records when a trigger parks — the declared
/// [`ErrorClass`] outcome, plus the honest label for errors that were
/// never classified. `transient_exhausted` is distinguishable from
/// `poison` so a real outage's backlog is mass-replayable *as a class*;
/// `unclassified` is distinguishable from `domain` so "this consumer
/// never classifies its errors" is visible instead of a lying default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// A transient-classified error outlived the liveness-time ceiling.
    TransientExhausted,
    /// A poison-classified error (or a structurally deterministic
    /// failure: trigger deserialization, OCC-fence violation).
    Poison,
    /// A domain-classified error exhausted its bounded attempts.
    Domain,
    /// An unclassified error exhausted its bounded attempts (domain
    /// policy, honest label).
    Unclassified,
}

impl FailureClass {
    /// The wire label, as written into terminal-failure facts.
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::TransientExhausted => "transient_exhausted",
            FailureClass::Poison => "poison",
            FailureClass::Domain => "domain",
            FailureClass::Unclassified => "unclassified",
        }
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error carrying its declared [`ErrorClass`]. Constructed via
/// [`transient`] / [`poison`] / [`domain`]; the reactor runner
/// downcasts through the `anyhow` chain to find it (the outermost
/// classification wins, so re-wrapping overrides).
#[derive(Debug)]
pub struct ClassifiedError {
    pub class: ErrorClass,
    pub source: anyhow::Error,
}

impl fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The class is carried structurally (TerminalFailure.class);
        // Display forwards the underlying message so log lines and
        // failure records read naturally.
        fmt::Display::fmt(&self.source, f)
    }
}

impl std::error::Error for ClassifiedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Classify an error as **transient** — an infrastructure blip the
/// runner should wait out: capped backoff up to the liveness-time
/// ceiling, then `transient_exhausted`.
///
/// ```ignore
/// let page = http.fetch(url).await.map_err(causal::transient)?;
/// ```
pub fn transient<E: Into<anyhow::Error>>(e: E) -> anyhow::Error {
    anyhow::Error::new(ClassifiedError { class: ErrorClass::Transient, source: e.into() })
}

/// Classify an error as **poison** — deterministic; the trigger parks
/// immediately because retrying reproduces the same failure.
pub fn poison<E: Into<anyhow::Error>>(e: E) -> anyhow::Error {
    anyhow::Error::new(ClassifiedError { class: ErrorClass::Poison, source: e.into() })
}

/// Classify an error as **domain** — the operation itself failed
/// meaningfully; bounded attempts, then a terminal-failure fact.
/// (Also the *policy* applied to unclassified errors, which park
/// labeled `unclassified` rather than `domain`.)
pub fn domain<E: Into<anyhow::Error>>(e: E) -> anyhow::Error {
    anyhow::Error::new(ClassifiedError { class: ErrorClass::Domain, source: e.into() })
}

/// Find the outermost declared classification in an error chain.
/// `None` = unclassified (domain policy, honest label).
pub(crate) fn classify(e: &anyhow::Error) -> Option<ErrorClass> {
    e.chain()
        .find_map(|c| c.downcast_ref::<ClassifiedError>().map(|c| c.class))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn classification_survives_context_wrapping() {
        let e = transient(anyhow!("connection refused"));
        let wrapped = e.context("while enriching job 42");
        assert_eq!(classify(&wrapped), Some(ErrorClass::Transient));
        assert!(format!("{wrapped:#}").contains("connection refused"));
    }

    #[test]
    fn outermost_classification_wins() {
        let inner = transient(anyhow!("looked transient"));
        let rewrapped = poison(inner);
        assert_eq!(classify(&rewrapped), Some(ErrorClass::Poison));
    }

    #[test]
    fn plain_errors_are_unclassified() {
        assert_eq!(classify(&anyhow!("boom")), None);
    }

    #[test]
    fn failure_class_labels_are_the_wire_contract() {
        assert_eq!(FailureClass::TransientExhausted.as_str(), "transient_exhausted");
        assert_eq!(FailureClass::Poison.as_str(), "poison");
        assert_eq!(FailureClass::Domain.as_str(), "domain");
        assert_eq!(FailureClass::Unclassified.as_str(), "unclassified");
    }
}
