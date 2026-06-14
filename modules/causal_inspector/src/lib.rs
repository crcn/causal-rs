pub mod types;
pub mod display;
pub mod read_model;

#[cfg(feature = "memory")]
pub mod memory;

#[cfg(feature = "memory")]
pub use memory::MemoryInspectorReadModel;

#[cfg(feature = "cache")]
pub mod cache;

#[cfg(feature = "graphql")]
pub mod graphql;

#[cfg(feature = "graphql")]
pub use graphql::{CausalInspectorQuery, CausalInspectorSubscription};

#[cfg(feature = "axum")]
mod axum_router;

#[cfg(feature = "axum")]
pub use axum_router::router;

pub use types::*;
pub use display::{EventDisplay, DefaultEventDisplay};
pub use read_model::{
    AggregateKeyEntry, AggregateKeysPage, AggregateStateSnapshotEntry, EffectRecord, EventQuery,
    InspectorReadModel, ReactorDescriptionEntry, ReactorDescriptionSnapshotEntry, ReactorLogEntry,
    ReactorOutcomeEntry, StoredEvent, SubjectChainEventRaw, SubjectChainMode, SubjectChainPage,
    SubjectChainSourceMode,
};
