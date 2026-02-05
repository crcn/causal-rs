//! Sampling strategies for span collection
//!
//! Sampling reduces overhead by only collecting a subset of spans.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Trait for sampling strategies
pub trait SamplingStrategy: Send + Sync {
    /// Decide whether to sample this event
    fn should_sample(&self, event_type: &str, attributes: &HashMap<String, String>) -> bool;
}

/// Always sample (collect every event)
pub struct AlwaysSample;

impl SamplingStrategy for AlwaysSample {
    fn should_sample(&self, _event_type: &str, _attributes: &HashMap<String, String>) -> bool {
        true
    }
}

/// Never sample (disable collection)
pub struct NeverSample;

impl SamplingStrategy for NeverSample {
    fn should_sample(&self, _event_type: &str, _attributes: &HashMap<String, String>) -> bool {
        false
    }
}

/// Sample at a fixed rate (1 in N events)
pub struct RateSample {
    rate: u64,
    counter: AtomicU64,
}

impl RateSample {
    /// Create a rate sampler that samples 1 in `rate` events
    ///
    /// Example: `RateSample::new(10)` samples 10% of events
    pub fn new(rate: u64) -> Self {
        Self {
            rate,
            counter: AtomicU64::new(0),
        }
    }
}

impl SamplingStrategy for RateSample {
    fn should_sample(&self, _event_type: &str, _attributes: &HashMap<String, String>) -> bool {
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        count % self.rate == 0
    }
}

/// Sample based on event type (allowlist)
pub struct EventTypeSampler {
    allowed_types: Vec<String>,
}

impl EventTypeSampler {
    pub fn new(allowed_types: Vec<String>) -> Self {
        Self { allowed_types }
    }
}

impl SamplingStrategy for EventTypeSampler {
    fn should_sample(&self, event_type: &str, _attributes: &HashMap<String, String>) -> bool {
        self.allowed_types.iter().any(|t| event_type.contains(t))
    }
}

/// Sample based on attribute presence
pub struct AttributeSampler {
    required_key: String,
}

impl AttributeSampler {
    pub fn new(required_key: String) -> Self {
        Self { required_key }
    }
}

impl SamplingStrategy for AttributeSampler {
    fn should_sample(&self, _event_type: &str, attributes: &HashMap<String, String>) -> bool {
        attributes.contains_key(&self.required_key)
    }
}

/// Composite sampler: ALL strategies must agree
pub struct AllOfSampler {
    strategies: Vec<Arc<dyn SamplingStrategy>>,
}

impl AllOfSampler {
    pub fn new(strategies: Vec<Arc<dyn SamplingStrategy>>) -> Self {
        Self { strategies }
    }
}

impl SamplingStrategy for AllOfSampler {
    fn should_sample(&self, event_type: &str, attributes: &HashMap<String, String>) -> bool {
        self.strategies
            .iter()
            .all(|s| s.should_sample(event_type, attributes))
    }
}

/// Composite sampler: ANY strategy can agree
pub struct AnyOfSampler {
    strategies: Vec<Arc<dyn SamplingStrategy>>,
}

impl AnyOfSampler {
    pub fn new(strategies: Vec<Arc<dyn SamplingStrategy>>) -> Self {
        Self { strategies }
    }
}

impl SamplingStrategy for AnyOfSampler {
    fn should_sample(&self, event_type: &str, attributes: &HashMap<String, String>) -> bool {
        self.strategies
            .iter()
            .any(|s| s.should_sample(event_type, attributes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_always_sample() {
        let sampler = AlwaysSample;
        assert!(sampler.should_sample("TestEvent", &HashMap::new()));
    }

    #[test]
    fn test_never_sample() {
        let sampler = NeverSample;
        assert!(!sampler.should_sample("TestEvent", &HashMap::new()));
    }

    #[test]
    fn test_rate_sample() {
        let sampler = RateSample::new(2); // Sample 50%

        let results: Vec<bool> = (0..10)
            .map(|_| sampler.should_sample("TestEvent", &HashMap::new()))
            .collect();

        let sampled = results.iter().filter(|&&x| x).count();
        assert_eq!(sampled, 5); // 50% of 10
    }

    #[test]
    fn test_event_type_sampler() {
        let sampler = EventTypeSampler::new(vec!["Order".into(), "Payment".into()]);

        assert!(sampler.should_sample("OrderPlaced", &HashMap::new()));
        assert!(sampler.should_sample("PaymentProcessed", &HashMap::new()));
        assert!(!sampler.should_sample("UserLoggedIn", &HashMap::new()));
    }

    #[test]
    fn test_all_of_sampler() {
        let sampler = AllOfSampler::new(vec![
            Arc::new(AlwaysSample),
            Arc::new(EventTypeSampler::new(vec!["Order".into()])),
        ]);

        assert!(sampler.should_sample("OrderPlaced", &HashMap::new()));
        assert!(!sampler.should_sample("UserLoggedIn", &HashMap::new()));
    }

    #[test]
    fn test_any_of_sampler() {
        let sampler = AnyOfSampler::new(vec![
            Arc::new(NeverSample),
            Arc::new(EventTypeSampler::new(vec!["Order".into()])),
        ]);

        assert!(sampler.should_sample("OrderPlaced", &HashMap::new()));
        assert!(!sampler.should_sample("UserLoggedIn", &HashMap::new()));
    }
}
