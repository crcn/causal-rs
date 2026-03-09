use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;

/// Internal codec for converting between queue payload JSON and typed events.
#[derive(Clone)]
pub(crate) struct EventCodec {
    /// Event prefix for codec lookup (e.g. "scrape", "order_placed").
    pub event_prefix: String,
    pub type_id: TypeId,
    pub decode: Arc<
        dyn Fn(&serde_json::Value) -> Result<Arc<dyn Any + Send + Sync>> + Send + Sync + 'static,
    >,
}
