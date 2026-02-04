//! Effect macros for ergonomic multi-variant matching.

/// Create multiple effects from pattern-matched event variants.
///
/// This macro provides a concise way to handle multiple event variants
/// with separate effects, avoiding verbose `.extract()` boilerplate.
///
/// # Example
///
/// ```ignore
/// use seesaw::on;
///
/// let effects = on!(CrawlEvent {
///     // Multiple patterns with | - same fields required
///     WebsiteIngested { website_id, job_id, .. } |
///     WebsitePostsRegenerated { website_id, job_id, .. } => |ctx| async move {
///         ctx.deps().jobs.enqueue(ExtractPostsJob {
///             website_id,
///             parent_job_id: job_id,
///         }).await?;
///         Ok(CrawlEvent::ExtractJobEnqueued { website_id })
///     },
///
///     // Single pattern
///     PostsExtractedFromPages { website_id, posts, .. } => |ctx| async move {
///         ctx.deps().jobs.enqueue(SyncPostsJob { website_id, posts }).await?;
///         Ok(CrawlEvent::SyncJobEnqueued { website_id })
///     },
/// });
///
/// // Add all effects to engine
/// let engine = effects.into_iter().fold(Engine::new(), |e, eff| e.with_effect(eff));
/// ```
///
/// # Generated Code
///
/// Each arm generates an effect equivalent to:
///
/// ```ignore
/// effect::on::<Event>()
///     .extract(|e| match e {
///         Event::Variant { field1, field2, .. } => Some((field1.clone(), field2.clone())),
///         _ => None,
///     })
///     .then(|(field1, field2), ctx| async move { ... })
/// ```
#[macro_export]
macro_rules! on {
    // Single variant arm
    (@arm $event:ident;
        [ $variant:ident { $($field:ident),* $(,)? .. } ]
        => |$ctx:ident| $body:expr
    ) => {
        $crate::effect::on::<$event>()
            .extract(|__e| match __e {
                $event::$variant { $($field),*, .. } => Some(($($field.clone()),*)),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .then(|($($field),*), $ctx| $body)
    };

    // Two variant arm (with |)
    (@arm $event:ident;
        [ $variant1:ident { $($field1:ident),* $(,)? .. } | $variant2:ident { $($field2:ident),* $(,)? .. } ]
        => |$ctx:ident| $body:expr
    ) => {
        $crate::effect::on::<$event>()
            .extract(|__e| match __e {
                $event::$variant1 { $($field1),*, .. } => Some(($($field1.clone()),*)),
                $event::$variant2 { $($field2),*, .. } => Some(($($field2.clone()),*)),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .then(|($($field1),*), $ctx| $body)
    };

    // Three variant arm (with |)
    (@arm $event:ident;
        [ $variant1:ident { $($field1:ident),* $(,)? .. } | $variant2:ident { $($field2:ident),* $(,)? .. } | $variant3:ident { $($field3:ident),* $(,)? .. } ]
        => |$ctx:ident| $body:expr
    ) => {
        $crate::effect::on::<$event>()
            .extract(|__e| match __e {
                $event::$variant1 { $($field1),*, .. } => Some(($($field1.clone()),*)),
                $event::$variant2 { $($field2),*, .. } => Some(($($field2.clone()),*)),
                $event::$variant3 { $($field3),*, .. } => Some(($($field3.clone()),*)),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .then(|($($field1),*), $ctx| $body)
    };

    // Four variant arm (with |)
    (@arm $event:ident;
        [ $variant1:ident { $($field1:ident),* $(,)? .. } | $variant2:ident { $($field2:ident),* $(,)? .. } | $variant3:ident { $($field3:ident),* $(,)? .. } | $variant4:ident { $($field4:ident),* $(,)? .. } ]
        => |$ctx:ident| $body:expr
    ) => {
        $crate::effect::on::<$event>()
            .extract(|__e| match __e {
                $event::$variant1 { $($field1),*, .. } => Some(($($field1.clone()),*)),
                $event::$variant2 { $($field2),*, .. } => Some(($($field2.clone()),*)),
                $event::$variant3 { $($field3),*, .. } => Some(($($field3.clone()),*)),
                $event::$variant4 { $($field4),*, .. } => Some(($($field4.clone()),*)),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .then(|($($field1),*), $ctx| $body)
    };

    // Entry: single variant pattern
    ($event:ident {
        $variant:ident { $($field:ident),* $(,)? .. } => |$ctx:ident| $body:expr
        $(,)?
    }) => {{
        vec![
            $crate::on!(@arm $event; [ $variant { $($field),* .. } ] => |$ctx| $body)
        ]
    }};

    // Entry: two arms, first is single variant
    ($event:ident {
        $variant1:ident { $($field1:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    }) => {{
        vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ]
    }};

    // Entry: two arms, first has two variants with |
    ($event:ident {
        $variant1a:ident { $($field1a:ident),* $(,)? .. } |
        $variant1b:ident { $($field1b:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    }) => {{
        vec![
            $crate::on!(@arm $event; [ $variant1a { $($field1a),* .. } | $variant1b { $($field1b),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ]
    }};

    // Entry: two arms, first has three variants with |
    ($event:ident {
        $variant1a:ident { $($field1a:ident),* $(,)? .. } |
        $variant1b:ident { $($field1b:ident),* $(,)? .. } |
        $variant1c:ident { $($field1c:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    }) => {{
        vec![
            $crate::on!(@arm $event; [ $variant1a { $($field1a),* .. } | $variant1b { $($field1b),* .. } | $variant1c { $($field1c),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ]
    }};

    // Entry: three arms
    ($event:ident {
        $variant1:ident { $($field1:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr,
        $variant3:ident { $($field3:ident),* $(,)? .. } => |$ctx3:ident| $body3:expr
        $(,)?
    }) => {{
        vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2),
            $crate::on!(@arm $event; [ $variant3 { $($field3),* .. } ] => |$ctx3| $body3)
        ]
    }};
}

#[cfg(test)]
mod tests {
    use crate::effect::Effect;
    use crate::EffectContext;
    use std::any::{Any, TypeId};
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct TestState;

    #[derive(Clone, Default)]
    struct TestDeps;

    #[derive(Clone, Debug)]
    enum TestEvent {
        VariantA { id: u64, data: String },
        VariantB { id: u64, data: String },
        VariantC { id: u64, items: Vec<String> },
        ResultA { id: u64 },
        ResultC { id: u64 },
    }

    fn create_test_ctx() -> EffectContext<TestState, TestDeps> {
        use crate::task_group::TaskGroup;
        use parking_lot::RwLock;

        struct NoopEmitter;
        impl crate::effect::context::EventEmitter<TestState, TestDeps> for NoopEmitter {
            fn emit(
                &self,
                _: TypeId,
                _: Arc<dyn Any + Send + Sync>,
                _: EffectContext<TestState, TestDeps>,
            ) {
            }
        }

        let state = Arc::new(TestState);
        let live_state = Arc::new(RwLock::new(TestState));
        EffectContext::new(
            state.clone(),
            state,
            live_state,
            Arc::new(TestDeps),
            Arc::new(NoopEmitter),
            TaskGroup::new(),
        )
    }

    #[test]
    fn test_on_macro_generates_effects() {
        let effects: Vec<Effect<TestState, TestDeps>> = on!(TestEvent {
            VariantA { id, data, .. } |
            VariantB { id, data, .. } => |_ctx| async move {
                let _ = (id, data);
                Ok(TestEvent::ResultA { id })
            },

            VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        });

        assert_eq!(effects.len(), 2);
    }

    #[test]
    fn test_on_macro_can_handle() {
        let effects: Vec<Effect<TestState, TestDeps>> = on!(TestEvent {
            VariantA { id, data, .. } |
            VariantB { id, data, .. } => |_ctx| async move {
                let _ = (id, data);
                Ok(TestEvent::ResultA { id })
            },

            VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        });

        // Both effects handle TestEvent
        for effect in &effects {
            assert!(effect.can_handle(TypeId::of::<TestEvent>()));
        }
    }

    #[tokio::test]
    async fn test_on_macro_first_arm_handles_variant_a() {
        let effects: Vec<Effect<TestState, TestDeps>> = on!(TestEvent {
            VariantA { id, data, .. } |
            VariantB { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },

            VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        });

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent::VariantA {
            id: 42,
            data: "test".into(),
        });

        // First effect should handle VariantA
        let result = effects[0]
            .call_handler(event.clone(), TypeId::of::<TestEvent>(), ctx.clone())
            .await
            .unwrap();

        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.type_id, TypeId::of::<TestEvent>());

        // Second effect should NOT handle VariantA (returns None)
        let result2 = effects[1]
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result2.is_none());
    }

    #[tokio::test]
    async fn test_on_macro_first_arm_handles_variant_b() {
        let effects: Vec<Effect<TestState, TestDeps>> = on!(TestEvent {
            VariantA { id, data, .. } |
            VariantB { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },

            VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        });

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent::VariantB {
            id: 99,
            data: "other".into(),
        });

        // First effect should also handle VariantB (via | pattern)
        let result = effects[0]
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_on_macro_second_arm_handles_variant_c() {
        let effects: Vec<Effect<TestState, TestDeps>> = on!(TestEvent {
            VariantA { id, data, .. } |
            VariantB { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },

            VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        });

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent::VariantC {
            id: 123,
            items: vec!["a".into(), "b".into()],
        });

        // First effect should NOT handle VariantC
        let result1 = effects[0]
            .call_handler(event.clone(), TypeId::of::<TestEvent>(), ctx.clone())
            .await
            .unwrap();
        assert!(result1.is_none());

        // Second effect should handle VariantC
        let result2 = effects[1]
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();
        assert!(result2.is_some());
    }

    #[test]
    fn test_on_macro_single_arm() {
        let effects: Vec<Effect<TestState, TestDeps>> = on!(TestEvent {
            VariantA { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            }
        });

        assert_eq!(effects.len(), 1);
        assert!(effects[0].can_handle(TypeId::of::<TestEvent>()));
    }
}
