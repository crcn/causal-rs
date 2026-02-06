use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;

/// Internal codec for converting between queue payload JSON and typed events.
#[derive(Clone)]
pub(crate) struct EventCodec {
    pub event_type: String,
    pub type_id: TypeId,
    pub decode: Arc<
        dyn Fn(&serde_json::Value) -> Result<Arc<dyn Any + Send + Sync>> + Send + Sync + 'static,
    >,
    pub encode: Arc<dyn Fn(&dyn Any) -> Option<serde_json::Value> + Send + Sync + 'static>,
}
