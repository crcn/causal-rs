/// Customizes how events are displayed in the inspector.
/// Implement this trait to provide domain-specific naming and summaries.
pub trait EventDisplay: Send + Sync {
    /// Human-readable name for an event (e.g. "Order Placed").
    fn display_name(&self, event_type: &str, payload: &serde_json::Value) -> String;

    /// Optional one-line summary for the timeline view.
    fn summary(&self, event_type: &str, payload: &serde_json::Value) -> Option<String>;
}

/// Default implementation — uses the payload's "type" field as the name.
pub struct DefaultEventDisplay;

impl EventDisplay for DefaultEventDisplay {
    fn display_name(&self, event_type: &str, payload: &serde_json::Value) -> String {
        payload
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or(event_type)
            .to_string()
    }

    fn summary(&self, _event_type: &str, _payload: &serde_json::Value) -> Option<String> {
        None
    }
}
