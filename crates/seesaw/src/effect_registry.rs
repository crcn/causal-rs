//! Effect registry for storing and starting effects.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::effect::Effect;
use crate::event_codec::EventCodec;

/// Registry for storing effects.
pub struct EffectRegistry<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    effects: RwLock<Vec<Effect<S, D>>>,
}

impl<S, D> Default for EffectRegistry<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S, D> EffectRegistry<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Create a new empty effect registry.
    pub fn new() -> Self {
        Self {
            effects: RwLock::new(Vec::new()),
        }
    }

    /// Register an effect.
    pub fn register(&self, effect: Effect<S, D>) {
        if effect.id.trim().is_empty() {
            panic!("Effect ID cannot be empty");
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
    pub(crate) fn all(&self) -> Vec<Effect<S, D>> {
        self.effects.read().iter().cloned().collect()
    }

    /// Find effect by stable ID.
    pub(crate) fn find_by_id(&self, effect_id: &str) -> Option<Effect<S, D>> {
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
    use crate::effect;
    use std::any::TypeId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct EventA;

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct EventB;

    #[derive(Clone)]
    struct TestState;

    #[tokio::test]
    async fn test_effect_registry_registers_effects() {
        let registry: EffectRegistry<TestState, ()> = EffectRegistry::new();

        registry.register(effect::on::<EventA>().then(|_, _| async { Ok(()) }));

        assert_eq!(registry.effects.read().len(), 1);
    }

    #[tokio::test]
    async fn test_multiple_effects() {
        let registry: EffectRegistry<TestState, ()> = EffectRegistry::new();

        registry.register(effect::on::<EventA>().then(|_, _| async { Ok(()) }));
        registry.register(effect::on::<EventB>().then(|_, _| async { Ok(()) }));

        assert_eq!(registry.effects.read().len(), 2);
    }

    #[tokio::test]
    async fn test_effect_can_handle() {
        let registry: EffectRegistry<TestState, ()> = EffectRegistry::new();

        registry.register(effect::on::<EventA>().then(|_, _| async { Ok(()) }));

        let effects = registry.effects.read();
        assert!(effects[0].can_handle(TypeId::of::<EventA>()));
        assert!(!effects[0].can_handle(TypeId::of::<EventB>()));
    }

    #[tokio::test]
    async fn test_group_effect() {
        let registry: EffectRegistry<TestState, ()> = EffectRegistry::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let counter_a = counter.clone();
        let counter_b = counter.clone();

        registry.register(effect::group([
            effect::on::<EventA>().then(move |_, _| {
                let c = counter_a.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }),
            effect::on::<EventB>().then(move |_, _| {
                let c = counter_b.clone();
                async move {
                    c.fetch_add(10, Ordering::SeqCst);
                    Ok(())
                }
            }),
        ]));

        // Single grouped effect registered
        assert_eq!(registry.effects.read().len(), 1);

        // Can handle both event types
        let effects = registry.effects.read();
        assert!(effects[0].can_handle(TypeId::of::<EventA>()));
        assert!(effects[0].can_handle(TypeId::of::<EventB>()));
    }

    #[tokio::test]
    #[should_panic(expected = "Duplicate effect id 'duplicate'")]
    async fn test_register_rejects_duplicate_effect_ids() {
        let registry: EffectRegistry<TestState, ()> = EffectRegistry::new();

        registry.register(
            effect::on::<EventA>()
                .id("duplicate")
                .then(|_, _| async { Ok(()) }),
        );
        registry.register(
            effect::on::<EventB>()
                .id("duplicate")
                .then(|_, _| async { Ok(()) }),
        );
    }

    #[tokio::test]
    #[should_panic(expected = "must declare an explicit stable id")]
    async fn test_register_rejects_generated_id_for_queued_effect() {
        let registry: EffectRegistry<TestState, ()> = EffectRegistry::new();
        registry.register(
            effect::on::<EventA>()
                .retry(3)
                .then(|_, _| async { Ok(()) }),
        );
    }
}
