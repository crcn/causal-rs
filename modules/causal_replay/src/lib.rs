//! # causal_replay
//!
//! Durable backends for the [`causal`] runtime, plus the cross-backend
//! conformance suite and replay/mirroring utilities.
//!
//! ## What lives here
//!
//! - **Event logs** — [`KurrentEventLogBackend`] (feature `kurrent`) and
//!   [`PgEventLogBackend`] (feature `postgres`), both implementing
//!   `causal::EventLogBackend`.
//! - **Cursors & snapshots** — [`PgReactorCheckpoint`] (projector/reactor
//!   cursors + retry counters), [`PgSnapshotStore`] (aggregate snapshots).
//! - **Observability** — [`PgReactorObserver`], [`PgInspectorReadModel`],
//!   and [`PgEventProjector`] (mirrors the durable log into Postgres
//!   `causal_log` for the inspector).
//! - **Conformance** — [`conformance`]: every `EventLogBackend` impl runs
//!   the same scenario suite (append idempotency on `event_id`, CAS via
//!   `StreamState`, monotonic revisions, strict-after reads, isolation).
//! - **Replay** — [`ProjectionStream`] / [`PointerStore`]: blue/green
//!   read-model rebuilds driven by the `REPLAY` env var.
//!
//! ## Recommended production shape (hybrid)
//!
//! KurrentDB as the event log; Postgres for the cursor/snapshot work,
//! which is inherently relational:
//!
//! ```no_run
//! # #[cfg(all(feature = "postgres", feature = "kurrent"))]
//! # async fn wire(pool: sqlx::PgPool) -> anyhow::Result<()> {
//! use std::sync::Arc;
//! use causal::{CheckpointStore, EngineBuilder, EventLogBackend, ReactorCheckpoint};
//! use causal_replay::{KurrentEventLogBackend, PgReactorCheckpoint};
//!
//! let kurrent = KurrentEventLogBackend::connect("kurrentdb://localhost:2113?tls=false")?;
//! let pg = Arc::new(PgReactorCheckpoint::new(pool));
//!
//! let engine = EngineBuilder::new(
//!     Arc::new(kurrent) as Arc<dyn EventLogBackend>,
//!     pg.clone() as Arc<dyn CheckpointStore>,
//!     pg as Arc<dyn ReactorCheckpoint>,
//! ).build();
//! # let _ = engine;
//! # Ok(())
//! # }
//! ```
//!
//! The Postgres schema is `docs/schema.sql` in the repository; apply it
//! via your migration runner — backends never auto-create tables.
//!
//! ## Replay mode
//!
//! Replay is a lifecycle state of the application, not an external tool.
//! The same `apply()` function runs in both live and replay mode —
//! [`ProjectionStream::run`] checks the `REPLAY` env var internally:
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
pub mod reconcile;
pub mod snapshot_store;
pub mod stream;
pub mod tail;

pub use mirroring::MirroringEventLogBackend;

pub use pointer::{PointerStatus, PointerStore};
pub use reconcile::{reconcile, Reconciliation};
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
