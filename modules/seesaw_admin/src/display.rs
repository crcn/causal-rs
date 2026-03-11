/// Customizes how events are displayed in the admin UI.
/// Implement this trait to provide domain-specific naming and summaries.
pub trait EventDisplay: Send + Sync {
    /// Human-readable name for an event (e.g. "discovery:source_discovered").
    fn display_name(&self, event_type: &str, payload: &serde_json::Value) -> String;

    /// Optional one-line summary for the timeline view.
    fn summary(&self, event_type: &str, payload: &serde_json::Value) -> Option<String>;

    /// Layer classification for filtering (e.g. "world", "system", "telemetry").
    fn layer(&self, event_type: &str) -> &str;
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

    fn layer(&self, _event_type: &str) -> &str {
        "system"
    }
}
