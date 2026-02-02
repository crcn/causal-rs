//! Action result types for service layer.

use anyhow::Result;
use async_trait::async_trait;

/// Trait for action return values that can be read asynchronously.
#[async_trait]
pub trait ActionResult<Data>: Send + Sync + 'static
where
    Data: Send + Sync + 'static,
{
    async fn read(&self) -> Result<Data>;
}

/// Generic action result wrapper for any cloneable value.
pub struct GenericActionResult<Value> {
    pub value: Value,
}

impl<Value: Send + Sync + Clone + 'static> GenericActionResult<Value> {
    pub fn new(value: Value) -> Self {
        Self { value }
    }
}

impl<Value> From<Value> for GenericActionResult<Value> {
    fn from(value: Value) -> Self {
        Self { value }
    }
}

#[async_trait]
impl<Value: Send + Sync + Clone + 'static> ActionResult<Value> for GenericActionResult<Value> {
    async fn read(&self) -> Result<Value> {
        Ok(self.value.clone())
    }
}

/// Empty result for actions that return nothing.
#[derive(Default, Clone)]
pub struct EmptyResult {}

#[async_trait]
impl ActionResult<()> for EmptyResult {
    async fn read(&self) -> Result<()> {
        Ok(())
    }
}
