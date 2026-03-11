//! Reactor registry for storing and managing reactors.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::event_codec::EventCodec;
use crate::reactor::{Reactor, Projection};

/// Registry for storing reactors.
pub struct ReactorRegistry<D>
where
    D: Send + Sync + 'static,
{
    reactors: RwLock<Vec<Reactor<D>>>,
    projections: RwLock<Vec<Projection<D>>>,
    /// Standalone codecs registered independently of reactors (e.g. from aggregators).
    standalone_codecs: RwLock<Vec<Arc<EventCodec>>>,
}

impl<D> Default for ReactorRegistry<D>
where
    D: Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            reactors: RwLock::new(Vec::new()),
            projections: RwLock::new(Vec::new()),
            standalone_codecs: RwLock::new(Vec::new()),
        }
    }
}

impl<D> ReactorRegistry<D>
where
    D: Send + Sync + 'static,
{
    /// Create a new empty reactor registry.
    pub fn new() -> Self {
        Self {
            reactors: RwLock::new(Vec::new()),
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

    /// Register a reactor.
    pub fn register(&self, reactor: Reactor<D>) {
        if reactor.id.trim().is_empty() {
            panic!("Reactor ID cannot be empty");
        }

        if !reactor.is_default() && looks_like_auto_generated_id(&reactor.id) {
            panic!(
                "Background reactor '{}' must declare an explicit stable id (for example .id(\"...\") or #[reactor(id = \"...\")])",
                reactor.id
            );
        }

        let mut reactors = self.reactors.write();
        if reactors.iter().any(|existing| existing.id == reactor.id) {
            panic!("Duplicate reactor id '{}'", reactor.id);
        }
        reactors.push(reactor);
    }

    /// Register a projection.
    pub fn register_projection(&self, projection: Projection<D>) {
        if projection.id.trim().is_empty() {
            panic!("Projection ID cannot be empty");
        }

        // Check uniqueness across both reactors and projections
        let reactors = self.reactors.read();
        if reactors.iter().any(|existing| existing.id == projection.id) {
            panic!("Duplicate id '{}' (conflicts with a reactor)", projection.id);
        }
        drop(reactors);

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

    /// Get all registered reactors (cloned).
    pub(crate) fn all(&self) -> Vec<Reactor<D>> {
        self.reactors.read().iter().cloned().collect()
    }

    /// Find reactor by stable ID.
    pub(crate) fn find_by_id(&self, reactor_id: &str) -> Option<Reactor<D>> {
        self.reactors
            .read()
            .iter()
            .find(|h| h.id == reactor_id)
            .cloned()
    }

    /// Find queue codec by durable name (prefix-based lookup).
    ///
    /// Extracts the prefix from the durable name (e.g. "scrape" from
    /// "scrape:web_scrape_completed") and matches against registered codecs.
    pub(crate) fn find_codec_by_durable_name(&self, durable_name: &str) -> Option<Arc<EventCodec>> {
        let prefix = crate::reactor::extract_prefix(durable_name);
        for reactor in self.reactors.read().iter() {
            for codec in reactor.codecs() {
                if codec.event_prefix == prefix {
                    return Some(codec.clone());
                }
            }
        }
        for codec in self.standalone_codecs.read().iter() {
            if codec.event_prefix == prefix {
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
    use crate::reactor;
    use std::any::TypeId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[causal_core_macros::event]
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct EventA;

    #[causal_core_macros::event]
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct EventB;

    #[derive(Clone, Default)]
    struct TestDeps;

    #[tokio::test]
    async fn test_reactor_registry_registers_handlers() {
        let registry: ReactorRegistry<TestDeps> = ReactorRegistry::new();

        registry.register(reactor::on::<EventA>().then(|_, _| async { Ok(crate::Events::new()) }));

        assert_eq!(registry.reactors.read().len(), 1);
    }

    #[tokio::test]
    async fn test_multiple_handlers() {
        let registry: ReactorRegistry<TestDeps> = ReactorRegistry::new();

        registry.register(reactor::on::<EventA>().then(|_, _| async { Ok(crate::Events::new()) }));
        registry.register(reactor::on::<EventB>().then(|_, _| async { Ok(crate::Events::new()) }));

        assert_eq!(registry.reactors.read().len(), 2);
    }

    #[tokio::test]
    async fn test_handler_can_handle() {
        let registry: ReactorRegistry<TestDeps> = ReactorRegistry::new();

        registry.register(reactor::on::<EventA>().then(|_, _| async { Ok(crate::Events::new()) }));

        let reactors = registry.reactors.read();
        assert!(reactors[0].can_handle(TypeId::of::<EventA>()));
        assert!(!reactors[0].can_handle(TypeId::of::<EventB>()));
    }

    #[tokio::test]
    async fn test_group_handler() {
        let registry: ReactorRegistry<TestDeps> = ReactorRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let counter_a = counter.clone();
        let counter_b = counter.clone();

        registry.register(
            reactor::on::<EventA>().id("test_a").then(move |_, _| {
                let c = counter_a.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::Events::new())
                }
            })
        );
        registry.register(
            reactor::on::<EventB>().id("test_b").then(move |_, _| {
                let c = counter_b.clone();
                async move {
                    c.fetch_add(10, Ordering::SeqCst);
                    Ok(crate::Events::new())
                }
            })
        );

        assert_eq!(registry.reactors.read().len(), 2);

        // Each can handle its respective event type
        let reactors = registry.reactors.read();
        assert!(reactors.iter().any(|h| h.can_handle(TypeId::of::<EventA>())));
        assert!(reactors.iter().any(|h| h.can_handle(TypeId::of::<EventB>())));
    }

    #[tokio::test]
    #[should_panic(expected = "Duplicate reactor id 'duplicate'")]
    async fn test_register_rejects_duplicate_reactor_ids() {
        let registry: ReactorRegistry<TestDeps> = ReactorRegistry::new();

        registry.register(
            reactor::on::<EventA>()
                .id("duplicate")
                .then(|_, _| async { Ok(crate::Events::new()) }),
        );
        registry.register(
            reactor::on::<EventB>()
                .id("duplicate")
                .then(|_, _| async { Ok(crate::Events::new()) }),
        );
    }

    #[tokio::test]
    #[should_panic(expected = "Background reactor")]
    async fn test_register_rejects_generated_id_for_queued_handler() {
        let registry: ReactorRegistry<TestDeps> = ReactorRegistry::new();
        registry.register(
            reactor::on::<EventA>()
                .retry(3)
                .then(|_, _| async { Ok(crate::Events::new()) }),
        );
    }
}
