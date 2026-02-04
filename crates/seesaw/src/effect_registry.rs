//! Effect registry for storing and starting effects.

use std::sync::atomic::Ordering;

use parking_lot::RwLock;
use tracing::{info_span, Instrument};

use crate::effect::{Effect, EffectContext, EventEnvelope};
use pipedream::RelayReceiver;

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
        self.effects.write().push(effect);
    }

    /// Start all registered effects.
    ///
    /// This subscribes each effect to the event relay and spawns background
    /// tasks to handle events.
    pub(crate) fn start_effects(&self, relay: &RelayReceiver, ctx: &EffectContext<S, D>) {
        // Create a transient context - tasks here don't block settled()
        // but ARE cancelled when the session settles or cancel() is called
        let bg_ctx = ctx.transient();

        // Collect ready signals from effect background loops
        let mut ready_signals = Vec::new();

        for effect in self.effects.read().iter() {
            let effect = effect.clone();

            // Use tracked subscription so send() waits for handler to be spawned.
            // This prevents race where settled() returns before handlers are spawned.
            let (mut rx, handler_count) = relay.subscribe_tracked::<EventEnvelope<S, D>>();
            let ctx = ctx.clone();

            // Call started on the foreground context BEFORE spawning the background loop.
            // This ensures started() effects are tracked by settled().
            // Errors from started() are propagated to settled().
            if effect.started.is_some() {
                ctx.within({
                    let effect = effect.clone();
                    move |inner_ctx| async move {
                        if let Some(ref started) = effect.started {
                            started(inner_ctx).await
                        } else {
                            Ok(())
                        }
                    }
                });
            }

            // Create a ready signal channel
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            ready_signals.push(ready_rx);

            // Spawn on background TaskGroup - this tracks the JoinHandle
            // so it gets aborted when cancel() is called or session settles
            bg_ctx.within(move |_| {
                async move {
                    // Signal ready before entering the receive loop
                    let _ = ready_tx.send(());

                    // Listen for events until channel closes
                    while let Some(envelope) = rx.recv().await {
                        // Get tracker BEFORE spawning handler - complete after spawn
                        let tracker = rx.current_tracker();

                        // Only handle if this effect cares about this event type
                        if effect.can_handle(envelope.event_type) {
                            // Use envelope's ctx so handler is tracked on foreground
                            let effect = effect.clone();
                            let event = envelope.event.clone();
                            let event_type = envelope.event_type;
                            let event_id = envelope.ctx.current_event_id();

                            // Create tracing span for effect handling
                            let span = info_span!(
                                "seesaw.effect",
                                event_id = ?event_id,
                            );

                            envelope.ctx.within(move |handler_ctx| {
                                async move {
                                    // Run effect handler
                                    let result = (effect.handler)(event, event_type, handler_ctx.clone()).await?;

                                    // If effect returned an event, dispatch it
                                    if let Some(output) = result {
                                        handler_ctx.emit_any(output.value, output.type_id);
                                    }

                                    Ok(())
                                }
                                .instrument(span)
                            });
                        }

                        // Complete tracker AFTER handler is spawned into TaskGroup.
                        // This ensures send() doesn't return until handler is tracked.
                        if let Some(t) = tracker {
                            t.complete_one();
                        }
                        rx.clear_tracker();
                    }

                    // Decrement handler count when effect loop exits
                    handler_count.fetch_sub(1, Ordering::SeqCst);

                    Ok(())
                }
            });
        }

        // Spawn a task that waits for all effect loops to be ready
        // This ensures effects are receiving before any events can be emitted
        if !ready_signals.is_empty() {
            ctx.within(move |_| async move {
                for signal in ready_signals {
                    let _ = signal.await;
                }
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect;
    use std::any::TypeId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug, Clone)]
    struct EventA;

    #[derive(Debug, Clone)]
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
}
