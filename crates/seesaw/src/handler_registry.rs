//! Handler registry for storing and managing handlers.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::event_codec::EventCodec;
use crate::handler::{Handler, Projection};

/// Registry for storing handlers.
pub struct HandlerRegistry<D>
where
    D: Send + Sync + 'static,
{
    handlers: RwLock<Vec<Handler<D>>>,
    projections: RwLock<Vec<Projection<D>>>,
    /// Standalone codecs registered independently of handlers (e.g. from aggregators).
    standalone_codecs: RwLock<Vec<Arc<EventCodec>>>,
}

impl<D> Default for HandlerRegistry<D>
where
    D: Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            handlers: RwLock::new(Vec::new()),
            projections: RwLock::new(Vec::new()),
            standalone_codecs: RwLock::new(Vec::new()),
        }
    }
}

impl<D> HandlerRegistry<D>
where
    D: Send + Sync + 'static,
{
    /// Create a new empty handler registry.
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(Vec::new()),
            projections: RwLock::new(Vec::new()),
            standalone_codecs: RwLock::new(Vec::new()),
        }
    }

    /// Register a standalone event codec (e.g. for aggregator event types).
    pub(crate) fn register_codec(&self, codec: Arc<EventCodec>) {
        let codecs = self.standalone_codecs.read();
        // Skip if already registered for this type
        if codecs.iter().any(|c| c.type_id == codec.type_id) {
            return;
        }
        drop(codecs);
        self.standalone_codecs.write().push(codec);
    }

    /// Register a handler.
    pub fn register(&self, handler: Handler<D>) {
        if handler.id.trim().is_empty() {
            panic!("Handler ID cannot be empty");
        }

        if !handler.is_default() && looks_like_auto_generated_id(&handler.id) {
            panic!(
                "Background handler '{}' must declare an explicit stable id (for example .id(\"...\") or #[handler(id = \"...\")])",
                handler.id
            );
        }

        let mut handlers = self.handlers.write();
        if handlers.iter().any(|existing| existing.id == handler.id) {
            panic!("Duplicate handler id '{}'", handler.id);
        }
        handlers.push(handler);
    }

    /// Register a projection.
    pub fn register_projection(&self, projection: Projection<D>) {
        if projection.id.trim().is_empty() {
            panic!("Projection ID cannot be empty");
        }

        // Check uniqueness across both handlers and projections
        let handlers = self.handlers.read();
        if handlers.iter().any(|existing| existing.id == projection.id) {
            panic!("Duplicate id '{}' (conflicts with a handler)", projection.id);
        }
        drop(handlers);

        let mut projections = self.projections.write();
        if projections.iter().any(|existing| existing.id == projection.id) {
            panic!("Duplicate projection id '{}'", projection.id);
        }
        projections.push(projection);
    }

    /// Get all registered projections, sorted by priority (lower = first).
    pub(crate) fn projections(&self) -> Vec<Projection<D>> {
        let mut projections: Vec<_> = self.projections.read().iter().cloned().collect();
        projections.sort_by_key(|p| p.priority.unwrap_or(i32::MAX));
        projections
    }

    /// Get all registered handlers (cloned).
    pub(crate) fn all(&self) -> Vec<Handler<D>> {
        self.handlers.read().iter().cloned().collect()
    }

    /// Find handler by stable ID.
    pub(crate) fn find_by_id(&self, handler_id: &str) -> Option<Handler<D>> {
        self.handlers
            .read()
            .iter()
            .find(|h| h.id == handler_id)
            .cloned()
    }

    /// Find queue codec by event type name.
    pub(crate) fn find_codec_by_event_type(&self, event_type: &str) -> Option<Arc<EventCodec>> {
        for handler in self.handlers.read().iter() {
            for codec in handler.codecs() {
                if codec.event_type == event_type {
                    return Some(codec.clone());
                }
            }
        }
        for codec in self.standalone_codecs.read().iter() {
            if codec.event_type == event_type {
                return Some(codec.clone());
            }
        }
        None
    }

}

fn looks_like_auto_generated_id(id: &str) -> bool {
    let Some((_prefix, location)) = id.rsplit_once('@') else {
        return false;
    };
    let mut parts = location.rsplit(':');
    let Some(column) = parts.next() else {
        return false;
    };
    let Some(line) = parts.next() else {
        return false;
    };
    line.parse::<u32>().is_ok() && column.parse::<u32>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler;
    use std::any::TypeId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct EventA;

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct EventB;

    #[derive(Clone, Default)]
    struct TestDeps;

    #[tokio::test]
    async fn test_handler_registry_registers_handlers() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(handler::on::<EventA>().then(|_, _| async { Ok(crate::Events::new()) }));

        assert_eq!(registry.handlers.read().len(), 1);
    }

    #[tokio::test]
    async fn test_multiple_handlers() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(handler::on::<EventA>().then(|_, _| async { Ok(crate::Events::new()) }));
        registry.register(handler::on::<EventB>().then(|_, _| async { Ok(crate::Events::new()) }));

        assert_eq!(registry.handlers.read().len(), 2);
    }

    #[tokio::test]
    async fn test_handler_can_handle() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(handler::on::<EventA>().then(|_, _| async { Ok(crate::Events::new()) }));

        let handlers = registry.handlers.read();
        assert!(handlers[0].can_handle(TypeId::of::<EventA>()));
        assert!(!handlers[0].can_handle(TypeId::of::<EventB>()));
    }

    #[tokio::test]
    async fn test_group_handler() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let counter_a = counter.clone();
        let counter_b = counter.clone();

        registry.register(
            handler::on::<EventA>().id("test_a").then(move |_, _| {
                let c = counter_a.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::Events::new())
                }
            })
        );
        registry.register(
            handler::on::<EventB>().id("test_b").then(move |_, _| {
                let c = counter_b.clone();
                async move {
                    c.fetch_add(10, Ordering::SeqCst);
                    Ok(crate::Events::new())
                }
            })
        );

        assert_eq!(registry.handlers.read().len(), 2);

        // Each can handle its respective event type
        let handlers = registry.handlers.read();
        assert!(handlers.iter().any(|h| h.can_handle(TypeId::of::<EventA>())));
        assert!(handlers.iter().any(|h| h.can_handle(TypeId::of::<EventB>())));
    }

    #[tokio::test]
    #[should_panic(expected = "Duplicate handler id 'duplicate'")]
    async fn test_register_rejects_duplicate_handler_ids() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(
            handler::on::<EventA>()
                .id("duplicate")
                .then(|_, _| async { Ok(crate::Events::new()) }),
        );
        registry.register(
            handler::on::<EventB>()
                .id("duplicate")
                .then(|_, _| async { Ok(crate::Events::new()) }),
        );
    }

    #[tokio::test]
    #[should_panic(expected = "Background handler")]
    async fn test_register_rejects_generated_id_for_queued_handler() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();
        registry.register(
            handler::on::<EventA>()
                .retry(3)
                .then(|_, _| async { Ok(crate::Events::new()) }),
        );
    }
}
