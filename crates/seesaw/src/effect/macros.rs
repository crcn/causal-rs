//! Effect macros for ergonomic multi-variant matching.

/// Create an effect group from pattern-matched event variants.
///
/// This macro provides a concise way to handle multiple event variants
/// with separate effects, avoiding verbose `.extract()` boilerplate.
/// Returns a single grouped `Effect` that can be added directly to the engine.
///
/// # Example
///
/// ```ignore
/// use seesaw::on;
///
/// let effect = on! {
///     // Multiple patterns with | - same fields required
///     CrawlEvent::WebsiteIngested { website_id, job_id, .. } |
///     CrawlEvent::WebsitePostsRegenerated { website_id, job_id, .. } => |ctx| async move {
///         ctx.deps().jobs.enqueue(ExtractPostsJob {
///             website_id,
///             parent_job_id: job_id,
///         }).await?;
///         Ok(CrawlEvent::ExtractJobEnqueued { website_id })
///     },
///
///     // Single pattern
///     CrawlEvent::PostsExtractedFromPages { website_id, posts, .. } => |ctx| async move {
///         ctx.deps().jobs.enqueue(SyncPostsJob { website_id, posts }).await?;
///         Ok(CrawlEvent::SyncJobEnqueued { website_id })
///     },
/// };
///
/// // Add effect directly to engine
/// let engine = Engine::new().with_effect(effect);
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
    // Internal: Single variant arm (untyped)
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

    // Internal: Single variant arm (typed)
    (@arm $event:ident;
        [ $variant:ident { $($field:ident),* $(,)? .. } ]
        => |$ctx:ident : $ty:ty| $body:expr
    ) => {
        $crate::effect::on::<$event>()
            .extract(|__e| match __e {
                $event::$variant { $($field),*, .. } => Some(($($field.clone()),*)),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .then(|($($field),*), $ctx: $ty| $body)
    };

    // Internal: Two variant arm (untyped)
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

    // Internal: Two variant arm (typed)
    (@arm $event:ident;
        [ $variant1:ident { $($field1:ident),* $(,)? .. } | $variant2:ident { $($field2:ident),* $(,)? .. } ]
        => |$ctx:ident : $ty:ty| $body:expr
    ) => {
        $crate::effect::on::<$event>()
            .extract(|__e| match __e {
                $event::$variant1 { $($field1),*, .. } => Some(($($field1.clone()),*)),
                $event::$variant2 { $($field2),*, .. } => Some(($($field2.clone()),*)),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .then(|($($field1),*), $ctx: $ty| $body)
    };

    // Internal: Three variant arm (untyped)
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

    // Internal: Three variant arm (typed)
    (@arm $event:ident;
        [ $variant1:ident { $($field1:ident),* $(,)? .. } | $variant2:ident { $($field2:ident),* $(,)? .. } | $variant3:ident { $($field3:ident),* $(,)? .. } ]
        => |$ctx:ident : $ty:ty| $body:expr
    ) => {
        $crate::effect::on::<$event>()
            .extract(|__e| match __e {
                $event::$variant1 { $($field1),*, .. } => Some(($($field1.clone()),*)),
                $event::$variant2 { $($field2),*, .. } => Some(($($field2.clone()),*)),
                $event::$variant3 { $($field3),*, .. } => Some(($($field3.clone()),*)),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .then(|($($field1),*), $ctx: $ty| $body)
    };

    // Internal: Four variant arm (untyped)
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

    // Internal: Four variant arm (typed)
    (@arm $event:ident;
        [ $variant1:ident { $($field1:ident),* $(,)? .. } | $variant2:ident { $($field2:ident),* $(,)? .. } | $variant3:ident { $($field3:ident),* $(,)? .. } | $variant4:ident { $($field4:ident),* $(,)? .. } ]
        => |$ctx:ident : $ty:ty| $body:expr
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
            .then(|($($field1),*), $ctx: $ty| $body)
    };

    // ============================================================
    // New syntax: Event::Variant { ... } => |ctx| async move { ... }
    // Supports optional type annotation: |ctx: Type| async move { ... }
    // ============================================================

    // Single arm, single variant (untyped)
    {
        $event:ident :: $variant:ident { $($field:ident),* $(,)? .. } => |$ctx:ident| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant { $($field),* .. } ] => |$ctx| $body)
        ])
    }};

    // Single arm, single variant (typed)
    {
        $event:ident :: $variant:ident { $($field:ident),* $(,)? .. } => |$ctx:ident : $ty:ty| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant { $($field),* .. } ] => |$ctx: $ty| $body)
        ])
    }};

    // Single arm, two variants with | (untyped)
    {
        $event:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } |
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx:ident| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } | $variant2 { $($field2),* .. } ] => |$ctx| $body)
        ])
    }};

    // Single arm, two variants with | (typed)
    {
        $event:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } |
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx:ident : $ty:ty| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } | $variant2 { $($field2),* .. } ] => |$ctx: $ty| $body)
        ])
    }};

    // Single arm, three variants with | (untyped)
    {
        $event:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } |
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } |
        $event3:ident :: $variant3:ident { $($field3:ident),* $(,)? .. } => |$ctx:ident| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } | $variant2 { $($field2),* .. } | $variant3 { $($field3),* .. } ] => |$ctx| $body)
        ])
    }};

    // Single arm, three variants with | (typed)
    {
        $event:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } |
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } |
        $event3:ident :: $variant3:ident { $($field3:ident),* $(,)? .. } => |$ctx:ident : $ty:ty| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } | $variant2 { $($field2),* .. } | $variant3 { $($field3),* .. } ] => |$ctx: $ty| $body)
        ])
    }};

    // Single arm, four variants with | (untyped)
    {
        $event:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } |
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } |
        $event3:ident :: $variant3:ident { $($field3:ident),* $(,)? .. } |
        $event4:ident :: $variant4:ident { $($field4:ident),* $(,)? .. } => |$ctx:ident| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } | $variant2 { $($field2),* .. } | $variant3 { $($field3),* .. } | $variant4 { $($field4),* .. } ] => |$ctx| $body)
        ])
    }};

    // Single arm, four variants with | (typed)
    {
        $event:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } |
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } |
        $event3:ident :: $variant3:ident { $($field3:ident),* $(,)? .. } |
        $event4:ident :: $variant4:ident { $($field4:ident),* $(,)? .. } => |$ctx:ident : $ty:ty| $body:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } | $variant2 { $($field2),* .. } | $variant3 { $($field3),* .. } | $variant4 { $($field4),* .. } ] => |$ctx: $ty| $body)
        ])
    }};

    // Two arms, both single variant
    {
        $event1:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event1; [ $variant1 { $($field1),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event2; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ])
    }};

    // Two arms, first has two variants with |
    {
        $event1a:ident :: $variant1a:ident { $($field1a:ident),* $(,)? .. } |
        $event1b:ident :: $variant1b:ident { $($field1b:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event1a; [ $variant1a { $($field1a),* .. } | $variant1b { $($field1b),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event2; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ])
    }};

    // Two arms, first has three variants with |
    {
        $event1a:ident :: $variant1a:ident { $($field1a:ident),* $(,)? .. } |
        $event1b:ident :: $variant1b:ident { $($field1b:ident),* $(,)? .. } |
        $event1c:ident :: $variant1c:ident { $($field1c:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event1a; [ $variant1a { $($field1a),* .. } | $variant1b { $($field1b),* .. } | $variant1c { $($field1c),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event2; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ])
    }};

    // Three arms, all single variant
    {
        $event1:ident :: $variant1:ident { $($field1:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $event2:ident :: $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr,
        $event3:ident :: $variant3:ident { $($field3:ident),* $(,)? .. } => |$ctx3:ident| $body3:expr
        $(,)?
    } => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event1; [ $variant1 { $($field1),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event2; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2),
            $crate::on!(@arm $event3; [ $variant3 { $($field3),* .. } ] => |$ctx3| $body3)
        ])
    }};

    // ============================================================
    // Legacy syntax: Event { Variant { ... } => ... }
    // ============================================================

    // Entry: single variant pattern
    ($event:ident {
        $variant:ident { $($field:ident),* $(,)? .. } => |$ctx:ident| $body:expr
        $(,)?
    }) => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant { $($field),* .. } ] => |$ctx| $body)
        ])
    }};

    // Entry: two arms, first is single variant
    ($event:ident {
        $variant1:ident { $($field1:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    }) => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ])
    }};

    // Entry: two arms, first has two variants with |
    ($event:ident {
        $variant1a:ident { $($field1a:ident),* $(,)? .. } |
        $variant1b:ident { $($field1b:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    }) => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1a { $($field1a),* .. } | $variant1b { $($field1b),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ])
    }};

    // Entry: two arms, first has three variants with |
    ($event:ident {
        $variant1a:ident { $($field1a:ident),* $(,)? .. } |
        $variant1b:ident { $($field1b:ident),* $(,)? .. } |
        $variant1c:ident { $($field1c:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr
        $(,)?
    }) => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1a { $($field1a),* .. } | $variant1b { $($field1b),* .. } | $variant1c { $($field1c),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2)
        ])
    }};

    // Entry: three arms
    ($event:ident {
        $variant1:ident { $($field1:ident),* $(,)? .. } => |$ctx1:ident| $body1:expr,
        $variant2:ident { $($field2:ident),* $(,)? .. } => |$ctx2:ident| $body2:expr,
        $variant3:ident { $($field3:ident),* $(,)? .. } => |$ctx3:ident| $body3:expr
        $(,)?
    }) => {{
        $crate::effect::group(vec![
            $crate::on!(@arm $event; [ $variant1 { $($field1),* .. } ] => |$ctx1| $body1),
            $crate::on!(@arm $event; [ $variant2 { $($field2),* .. } ] => |$ctx2| $body2),
            $crate::on!(@arm $event; [ $variant3 { $($field3),* .. } ] => |$ctx3| $body3)
        ])
    }};
}

#[cfg(test)]
mod tests {
    use crate::effect::Effect;
    use crate::EffectContext;
    use std::any::{Any, TypeId};
    use std::sync::Arc;
    use uuid::Uuid;

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
            "test_effect".to_string(),
            "test_idempotency_key".to_string(),
            Uuid::nil(),
            Uuid::nil(),
            state.clone(),
            state,
            live_state,
            Arc::new(TestDeps),
            Arc::new(NoopEmitter),
            TaskGroup::new(),
        )
    }

    // ============================================================
    // New syntax tests: Event::Variant { ... } => ...
    // ============================================================

    #[test]
    fn test_on_macro_new_syntax_generates_grouped_effect() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } |
            TestEvent::VariantB { id, data, .. } => |_ctx| async move {
                let _ = (id, data);
                Ok(TestEvent::ResultA { id })
            },

            TestEvent::VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        };

        // Grouped effect handles TestEvent
        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    #[test]
    fn test_on_macro_new_syntax_can_handle() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } |
            TestEvent::VariantB { id, data, .. } => |_ctx| async move {
                let _ = (id, data);
                Ok(TestEvent::ResultA { id })
            },

            TestEvent::VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        };

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    #[tokio::test]
    async fn test_on_macro_new_syntax_handles_variant_a() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } |
            TestEvent::VariantB { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },

            TestEvent::VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        };

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent::VariantA {
            id: 42,
            data: "test".into(),
        });

        let result = effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.type_id, TypeId::of::<TestEvent>());
    }

    #[tokio::test]
    async fn test_on_macro_new_syntax_handles_variant_b() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } |
            TestEvent::VariantB { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },

            TestEvent::VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        };

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent::VariantB {
            id: 99,
            data: "other".into(),
        });

        let result = effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_on_macro_new_syntax_handles_variant_c() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } |
            TestEvent::VariantB { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },

            TestEvent::VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        };

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent::VariantC {
            id: 123,
            items: vec!["a".into(), "b".into()],
        });

        let result = effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
    }

    #[test]
    fn test_on_macro_new_syntax_single_arm() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            }
        };

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    #[test]
    fn test_on_macro_new_syntax_three_arms() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },
            TestEvent::VariantB { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            },
            TestEvent::VariantC { id, items, .. } => |_ctx| async move {
                let _ = items;
                Ok(TestEvent::ResultC { id })
            }
        };

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    // ============================================================
    // Legacy syntax tests: Event { Variant { ... } => ... }
    // ============================================================

    #[test]
    fn test_on_macro_legacy_generates_grouped_effect() {
        let effect: Effect<TestState, TestDeps> = on!(TestEvent {
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

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    #[test]
    fn test_on_macro_legacy_can_handle() {
        let effect: Effect<TestState, TestDeps> = on!(TestEvent {
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

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    #[tokio::test]
    async fn test_on_macro_legacy_handles_variant_a() {
        let effect: Effect<TestState, TestDeps> = on!(TestEvent {
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

        let result = effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.type_id, TypeId::of::<TestEvent>());
    }

    #[tokio::test]
    async fn test_on_macro_legacy_handles_variant_b() {
        let effect: Effect<TestState, TestDeps> = on!(TestEvent {
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

        let result = effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_on_macro_legacy_handles_variant_c() {
        let effect: Effect<TestState, TestDeps> = on!(TestEvent {
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

        let result = effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
    }

    #[test]
    fn test_on_macro_legacy_single_arm() {
        let effect: Effect<TestState, TestDeps> = on!(TestEvent {
            VariantA { id, data, .. } => |_ctx| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            }
        });

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    // ============================================================
    // Typed closure tests
    // ============================================================

    #[test]
    fn test_on_macro_typed_closure_single_variant() {
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } => |_ctx: EffectContext<TestState, TestDeps>| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            }
        };

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }

    #[test]
    fn test_on_macro_typed_closure_two_variants() {
        // When using |, all variants must have the same extracted field types
        let effect: Effect<TestState, TestDeps> = on! {
            TestEvent::VariantA { id, data, .. } |
            TestEvent::VariantB { id, data, .. } => |_ctx: EffectContext<TestState, TestDeps>| async move {
                let _ = data;
                Ok(TestEvent::ResultA { id })
            }
        };

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
    }
}
