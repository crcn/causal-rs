//! The `Event` trait — compile-time event identity for durable naming.
//!
//! Every event in seesaw must implement this trait (typically via the `#[event]` macro).
//! It provides:
//! - A **durable name** per variant/struct for storage and routing
//! - A **prefix** for codec/aggregator registration and lookup
//! - An **ephemeral** flag for events that skip persistence

/// Compile-time event identity.
///
/// Implemented automatically by the `#[event]` proc macro. Provides stable,
/// domain-prefixed names for storage and routing.
///
/// # Examples
///
/// ```ignore
/// // Enum with domain prefix
/// #[event(prefix = "scrape")]
/// #[derive(Clone, Serialize, Deserialize)]
/// #[serde(tag = "type", rename_all = "snake_case")]
/// pub enum ScrapeEvent {
///     WebScrapeCompleted { urls_scraped: usize },
///     SourcesResolved { sources: Vec<Uuid> },
/// }
/// // durable_name: "scrape:web_scrape_completed", "scrape:sources_resolved"
/// // event_prefix: "scrape"
///
/// // Plain struct
/// #[event]
/// #[derive(Clone, Serialize, Deserialize)]
/// pub struct OrderPlaced { pub order_id: Uuid }
/// // durable_name: "order_placed"
/// // event_prefix: "order_placed"
/// ```
pub trait Event:
    serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static
{
    /// The stable, domain-prefixed name for this specific event instance.
    ///
    /// - Enum: `"scrape:web_scrape_completed"` (varies by variant)
    /// - Struct: `"order_placed"` (always the same)
    fn durable_name(&self) -> &str;

    /// Type-level prefix for codec/aggregator registration and lookup.
    ///
    /// - Enum: `"scrape"` (shared by all variants)
    /// - Struct: `"order_placed"` (same as durable_name)
    fn event_prefix() -> &'static str;

    /// Whether this event is ephemeral (not persisted to EventLog).
    ///
    /// Ephemeral events route through handlers but skip persistence,
    /// aggregate apply, and projections.
    fn is_ephemeral() -> bool {
        false
    }
}
