//! Effect registry for storing and starting effects.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::event_codec::EventCodec;
use crate::handler::Handler;

/// Registry for storing effects.
pub struct HandlerRegistry<D>
where
    D: Send + Sync + 'static,
{
    effects: RwLock<Vec<Handler<D>>>,
    /// Standalone codecs registered independently of handlers (e.g. from aggregators).
    standalone_codecs: RwLock<Vec<Arc<EventCodec>>>,
}

impl<D> Default for HandlerRegistry<D>
where
    D: Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            effects: RwLock::new(Vec::new()),
            standalone_codecs: RwLock::new(Vec::new()),
        }
    }
}

impl<D> HandlerRegistry<D>
where
    D: Send + Sync + 'static,
{
    /// Create a new empty effect registry.
    pub fn new() -> Self {
        Self {
            effects: RwLock::new(Vec::new()),
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

    /// Register an effect.
    pub fn register(&self, effect: Handler<D>) {
        if effect.id.trim().is_empty() {
            panic!("Handler ID cannot be empty");
        }

        if !effect.is_inline() && looks_like_auto_generated_id(&effect.id) {
            panic!(
                "Queued effect '{}' must declare an explicit stable id (for example .id(\"...\") or #[effect(id = \"...\")])",
                effect.id
            );
        }

        let mut effects = self.effects.write();
        if effects.iter().any(|existing| existing.id == effect.id) {
            panic!("Duplicate effect id '{}'", effect.id);
        }
        effects.push(effect);
    }

    /// Get all registered effects (cloned).
    pub(crate) fn all(&self) -> Vec<Handler<D>> {
        self.effects.read().iter().cloned().collect()
    }

    /// Find effect by stable ID.
    pub(crate) fn find_by_id(&self, effect_id: &str) -> Option<Handler<D>> {
        self.effects
            .read()
            .iter()
            .find(|effect| effect.id == effect_id)
            .cloned()
    }

    /// Find queue codec by event type name.
    pub(crate) fn find_codec_by_event_type(&self, event_type: &str) -> Option<Arc<EventCodec>> {
        for effect in self.effects.read().iter() {
            for codec in effect.codecs() {
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

    /// Find queue codec by Rust TypeId.
    pub(crate) fn find_codec_by_type_id(
        &self,
        type_id: std::any::TypeId,
    ) -> Option<Arc<EventCodec>> {
        for effect in self.effects.read().iter() {
            for codec in effect.codecs() {
                if codec.type_id == type_id {
                    return Some(codec.clone());
                }
            }
        }
        for codec in self.standalone_codecs.read().iter() {
            if codec.type_id == type_id {
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
    async fn test_handler_registry_registers_effects() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(handler::on::<EventA>().then(|_, _| async { Ok(()) }));

        assert_eq!(registry.effects.read().len(), 1);
    }

    #[tokio::test]
    async fn test_multiple_effects() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(handler::on::<EventA>().then(|_, _| async { Ok(()) }));
        registry.register(handler::on::<EventB>().then(|_, _| async { Ok(()) }));

        assert_eq!(registry.effects.read().len(), 2);
    }

    #[tokio::test]
    async fn test_effect_can_handle() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(handler::on::<EventA>().then(|_, _| async { Ok(()) }));

        let effects = registry.effects.read();
        assert!(effects[0].can_handle(TypeId::of::<EventA>()));
        assert!(!effects[0].can_handle(TypeId::of::<EventB>()));
    }

    #[tokio::test]
    async fn test_group_effect() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let counter_a = counter.clone();
        let counter_b = counter.clone();

        registry.register(
            handler::on::<EventA>().id("test_a").then(move |_, _| {
                let c = counter_a.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
        );
        registry.register(
            handler::on::<EventB>().id("test_b").then(move |_, _| {
                let c = counter_b.clone();
                async move {
                    c.fetch_add(10, Ordering::SeqCst);
                    Ok(())
                }
            })
        );

        assert_eq!(registry.effects.read().len(), 2);

        // Each can handle its respective event type
        let effects = registry.effects.read();
        assert!(effects.iter().any(|e| e.can_handle(TypeId::of::<EventA>())));
        assert!(effects.iter().any(|e| e.can_handle(TypeId::of::<EventB>())));
    }

    #[tokio::test]
    #[should_panic(expected = "Duplicate effect id 'duplicate'")]
    async fn test_register_rejects_duplicate_effect_ids() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();

        registry.register(
            handler::on::<EventA>()
                .id("duplicate")
                .then(|_, _| async { Ok(()) }),
        );
        registry.register(
            handler::on::<EventB>()
                .id("duplicate")
                .then(|_, _| async { Ok(()) }),
        );
    }

    #[tokio::test]
    #[should_panic(expected = "must declare an explicit stable id")]
    async fn test_register_rejects_generated_id_for_queued_effect() {
        let registry: HandlerRegistry<TestDeps> = HandlerRegistry::new();
        registry.register(
            handler::on::<EventA>()
                .retry(3)
                .then(|_, _| async { Ok(()) }),
        );
    }
}
