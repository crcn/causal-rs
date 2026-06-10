//! # causal_replay
//!
//! Replay and projection library for causal event processing.
//!
//! Replay is a lifecycle state of the application, not an external tool.
//! The same `apply()` function runs in both live and replay mode —
//! `ProjectionStream::run()` checks the `REPLAY` env var internally.
//!
//! ## Quick Start
//!
//! ```ignore
//! use causal_replay::{ProjectionStream, PgPointerStore, PgNotifyTailSource};
//!
//! let pointer = PgPointerStore::new(db.clone()).await?;
//! let tail = PgNotifyTailSource::new(&db, "events").await?;
//!
//! let stream = ProjectionStream::new(&log, &pointer)
//!     .tail(Box::new(tail))
//!     .promote_if(|| health_check(&neo4j));
//!
//! let version = stream.version().await?;  // DB version for both modes
//! let neo4j = connect(&format!("neo4j.v{version}")).await?;
//!
//! stream.run(|event| projections.apply(event)).await?;
//! ```
//!
//! ```bash
//! $ server                                  # live: catch up, tail
//! $ REPLAY=1 server                         # replay: full read, promote, exit
//! $ REPLAY=1 REPLAY_TARGETS=neo4j server    # replay neo4j only
//! ```

pub mod conformance;
pub mod event_log;
pub mod kurrent_event_log;
pub mod event_projector;
pub mod inspector_read_model;
pub mod mirroring;
pub mod pointer;
pub mod reactor_checkpoint;
pub mod reactor_observer;
pub mod snapshot_store;
pub mod stream;
pub mod tail;

pub use mirroring::MirroringEventLogBackend;

pub use pointer::{PointerStatus, PointerStore};
pub use stream::{Mode, ProjectionStream, ReplayProgress};
pub use tail::{PollTailSource, TailSource};

#[cfg(feature = "postgres")]
pub use event_log::{PgEventLogBackend, ADVISORY_LOCK_CLASS, ADVISORY_LOCK_OBJID};

#[cfg(feature = "postgres")]
pub use pointer::PgPointerStore;

#[cfg(feature = "postgres")]
pub use reactor_checkpoint::PgReactorCheckpoint;

#[cfg(feature = "postgres")]
pub use reactor_observer::PgReactorObserver;

#[cfg(feature = "postgres")]
pub use inspector_read_model::PgInspectorReadModel;

#[cfg(feature = "postgres")]
pub use event_projector::PgEventProjector;

#[cfg(feature = "postgres")]
pub use snapshot_store::PgSnapshotStore;

#[cfg(feature = "postgres")]
pub use tail::PgNotifyTailSource;

#[cfg(feature = "kurrent")]
pub use kurrent_event_log::KurrentEventLogBackend;
