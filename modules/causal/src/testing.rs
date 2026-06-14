//! Test helpers — gated behind `#[cfg(any(test, feature = "testing"))]`.
//!
//! [`TestCtx`] owns the data that [`Ctx`] borrows, so tests can construct
//! a fully wired context without repeating the struct literal boilerplate.
//!
//! ## Usage
//!
//! ```ignore
//! use causal::testing::TestCtx;
//!
//! // Minimal — nil ids, no effect store.
//! let owner = TestCtx::new();
//! let ctx = owner.ctx();
//! assert_eq!(ctx.derive_id("label").unwrap(), ctx.derive_id("label").unwrap());
//!
//! // With memoized effects.
//! let owner = TestCtx::new().with_effects();
//! let ctx = owner.ctx();
//! let result = ctx.effect("fetch", || async { Ok::<String, anyhow::Error>("hi".into()) }).await?;
//! ```

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::contexts::{Ctx, LabelSet, Metadata, StateSource};
use crate::effect_store::{EffectStore, InMemoryEffectStore};
use crate::types::LogCursor;

/// Owned counterpart to [`Ctx`]. Holds all referenced data so a test can
/// call `.ctx()` to borrow a fully configured context.
pub struct TestCtx {
    pub metadata:    Metadata,
    pub labels:      LabelSet,
    pub event_id:    Uuid,
    pub workflow_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub consumer:    &'static str,
    effect_store:    Option<Arc<dyn EffectStore>>,
}

impl Default for TestCtx {
    fn default() -> Self {
        Self {
            metadata:    Metadata::new(),
            labels:      LabelSet::default(),
            event_id:    Uuid::nil(),
            workflow_id: Uuid::nil(),
            occurred_at: DateTime::UNIX_EPOCH.into(),
            consumer:    "test.consumer",
            effect_store: None,
        }
    }
}

impl TestCtx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire an [`InMemoryEffectStore`] so `ctx.effect()` and
    /// `ctx.effect_all()` work in the returned context.
    pub fn with_effects(mut self) -> Self {
        self.effect_store = Some(Arc::new(InMemoryEffectStore::new()));
        self
    }

    /// Borrow a [`Ctx`] from this owner. The lifetime is tied to `&self`
    /// so the owner must outlive any use of the returned context.
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx {
            event_id:            self.event_id,
            log_position:        LogCursor::ZERO,
            occurred_at:         self.occurred_at,
            workflow_id:         self.workflow_id,
            metadata:            &self.metadata,
            consumer:            self.consumer,
            labels:              Some(&self.labels),
            state:               StateSource::None,
            logs:                None,
            effect_store:        self.effect_store.as_ref(),
            cancelled_workflows: None,
        }
    }
}
