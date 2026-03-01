//! Upcaster registry for event schema evolution.
//!
//! When event shapes change (e.g., adding a `currency` field to `OrderPlaced`),
//! upcasters transform old persisted payloads to the current schema during
//! replay/decode — no data migration needed.
//!
//! Upcasters chain: v1 → v2 → v3 → current. Each registered upcaster transforms
//! from one schema version to the next.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

/// A single upcaster that transforms an event payload from one schema version to the next.
pub struct Upcaster {
    pub event_type: String,
    pub from_version: u32,
    pub transform: Arc<dyn Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync>,
}

/// Registry of upcasters keyed by short event type name.
pub struct UpcasterRegistry {
    upcasters: HashMap<String, Vec<Upcaster>>,
}

impl UpcasterRegistry {
    pub fn new() -> Self {
        Self {
            upcasters: HashMap::new(),
        }
    }

    pub fn register(&mut self, upcaster: Upcaster) {
        let entry = self.upcasters.entry(upcaster.event_type.clone()).or_default();
        entry.push(upcaster);
        entry.sort_by_key(|u| u.from_version);
    }

    /// Apply upcasters in sequence from `schema_version` to current.
    ///
    /// Applies all transforms where `from_version >= schema_version`.
    /// Returns the payload unchanged if no upcasters match.
    pub fn upcast(
        &self,
        event_type: &str,
        schema_version: u32,
        mut payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if let Some(chain) = self.upcasters.get(event_type) {
            for upcaster in chain {
                if upcaster.from_version >= schema_version {
                    payload = (upcaster.transform)(payload)?;
                }
            }
        }
        Ok(payload)
    }

    pub fn is_empty(&self) -> bool {
        self.upcasters.is_empty()
    }
}

impl Default for UpcasterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upcast_chain_v1_to_v3() {
        let mut registry = UpcasterRegistry::new();

        // v1 → v2: add currency field
        registry.register(Upcaster {
            event_type: "OrderPlaced".to_string(),
            from_version: 1,
            transform: Arc::new(|mut v| {
                v["currency"] = serde_json::json!("USD");
                Ok(v)
            }),
        });

        // v2 → v3: add region field
        registry.register(Upcaster {
            event_type: "OrderPlaced".to_string(),
            from_version: 2,
            transform: Arc::new(|mut v| {
                v["region"] = serde_json::json!("US");
                Ok(v)
            }),
        });

        // Upcast from v1 should apply both transforms
        let payload = serde_json::json!({"total": 100});
        let result = registry.upcast("OrderPlaced", 1, payload).unwrap();
        assert_eq!(result["total"], 100);
        assert_eq!(result["currency"], "USD");
        assert_eq!(result["region"], "US");

        // Upcast from v2 should apply only v2→v3
        let payload = serde_json::json!({"total": 100, "currency": "EUR"});
        let result = registry.upcast("OrderPlaced", 2, payload).unwrap();
        assert_eq!(result["currency"], "EUR"); // not overwritten — already at v2
        assert_eq!(result["region"], "US");
    }

    #[test]
    fn upcast_noop_when_current() {
        let mut registry = UpcasterRegistry::new();

        registry.register(Upcaster {
            event_type: "OrderPlaced".to_string(),
            from_version: 1,
            transform: Arc::new(|mut v| {
                v["currency"] = serde_json::json!("USD");
                Ok(v)
            }),
        });

        // schema_version=2 means already past the v1 upcaster
        let payload = serde_json::json!({"total": 100, "currency": "EUR"});
        let result = registry.upcast("OrderPlaced", 2, payload.clone()).unwrap();
        assert_eq!(result, payload);
    }

    #[test]
    fn upcast_noop_for_unknown_event() {
        let registry = UpcasterRegistry::new();
        let payload = serde_json::json!({"foo": "bar"});
        let result = registry.upcast("UnknownEvent", 0, payload.clone()).unwrap();
        assert_eq!(result, payload);
    }

    #[test]
    fn upcast_error_propagates() {
        let mut registry = UpcasterRegistry::new();

        registry.register(Upcaster {
            event_type: "Bad".to_string(),
            from_version: 1,
            transform: Arc::new(|_| Err(anyhow::anyhow!("transform failed"))),
        });

        let result = registry.upcast("Bad", 1, serde_json::json!({}));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("transform failed"));
    }
}
