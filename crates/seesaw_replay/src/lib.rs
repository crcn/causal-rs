//! # seesaw_replay
//!
//! Replay and projection library for seesaw event processing.
//!
//! Replay is a lifecycle state of the application, not an external tool.
//! The same `apply()` function runs in both live and replay mode —
//! `ProjectionStream::run()` checks the `REPLAY` env var internally.
//!
//! ## Quick Start
//!
//! ```ignore
//! use seesaw_replay::{ProjectionStream, PgPointerStore, PgNotifyTailSource};
//!
//! let pointer = PgPointerStore::new(db.clone()).await?;
//! let tail = PgNotifyTailSource::new(&db, "events").await?;
//!
//! ProjectionStream::new(&log, &pointer)
//!     .tail(Box::new(tail))
//!     .promote_if(|| health_check(&neo4j))
//!     .run(|event| projections.apply(event))
//!     .await?;
//! ```
//!
//! ```bash
//! $ server                                  # live: catch up, tail
//! $ REPLAY=1 server                         # replay: full read, promote, exit
//! $ REPLAY=1 REPLAY_TARGETS=neo4j server    # replay neo4j only
//! ```

pub mod pointer;
pub mod stream;
pub mod tail;

pub use pointer::{PointerStatus, PointerStore};
pub use stream::{Mode, ProjectionStream};
pub use tail::{PollTailSource, TailSource};

#[cfg(feature = "postgres")]
pub use pointer::PgPointerStore;

#[cfg(feature = "postgres")]
pub use tail::PgNotifyTailSource;
