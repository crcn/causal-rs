//! Projection stream — catch up + tail (live) or full replay + promote (replay).

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use causal::event_log::EventLog;
use causal::types::PersistedEvent;

use crate::pointer::PointerStore;
use crate::tail::{PollTailSource, TailSource};

const DEFAULT_BATCH_SIZE: usize = 1000;
const DEFAULT_CHECKPOINT_INTERVAL: usize = 1000;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

type PromoteGate =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<bool>> + Send>> + Send + Sync>;
type ProgressCallback = Box<dyn Fn(ReplayProgress) + Send + Sync>;

/// Progress during replay.
#[derive(Debug, Clone, Copy)]
pub struct ReplayProgress {
    /// Events processed so far.
    pub processed: usize,
    /// Target position (from `version()`).
    pub target: u64,
    /// Current position in the event log.
    pub position: u64,
}

/// Mode for projection stream execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Catch up from active pointer, then tail indefinitely.
    Live,
    /// Full replay from position 0, promote on success, exit.
    Replay,
}

impl Mode {
    /// Detect mode from environment. `REPLAY=1` → Replay, otherwise Live.
    pub fn from_env() -> Self {
        if std::env::var("REPLAY").is_ok() {
            Mode::Replay
        } else {
            Mode::Live
        }
    }
}

/// Unified projection stream for live tailing and replay.
///
/// In **live mode** (default): catches up from the active pointer, then tails
/// for new events indefinitely.
///
/// In **replay mode** (`REPLAY=1` env var): reads all events from position 0,
/// stages the final position, runs the optional `promote_if` gate, and exits.
///
/// Same `apply()` function in both modes. The app code doesn't branch.
pub struct ProjectionStream<'a> {
    log: &'a dyn EventLog,
    pointer: &'a dyn PointerStore,
    tail_source: Option<Box<dyn TailSource>>,
    promote_gate: Option<PromoteGate>,
    progress_callback: Option<ProgressCallback>,
    mode: Option<Mode>,
    poll_interval: Duration,
    batch_size: usize,
    checkpoint_interval: usize,
}

impl<'a> ProjectionStream<'a> {
    /// Create a new projection stream.
    pub fn new(log: &'a dyn EventLog, pointer: &'a dyn PointerStore) -> Self {
        Self {
            log,
            pointer,
            tail_source: None,
            promote_gate: None,
            progress_callback: None,
            mode: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
            batch_size: DEFAULT_BATCH_SIZE,
            checkpoint_interval: DEFAULT_CHECKPOINT_INTERVAL,
        }
    }

    /// Set a custom tail source for live mode.
    ///
    /// In replay mode, the tail source is ignored.
    pub fn tail(mut self, source: Box<dyn TailSource>) -> Self {
        self.tail_source = Some(source);
        self
    }

    /// Set a promotion gate for replay mode.
    ///
    /// After replay completes, the gate function runs. If it returns `Ok(true)`,
    /// `staged` is promoted to `active`. If `Ok(false)` or `Err`, the replay
    /// stays staged and `run()` returns an error.
    ///
    /// Without a gate, replay auto-promotes on completion.
    pub fn promote_if<G, Gf>(mut self, gate: G) -> Self
    where
        G: Fn() -> Gf + Send + Sync + 'static,
        Gf: Future<Output = Result<bool>> + Send + 'static,
    {
        self.promote_gate = Some(Box::new(move || Box::pin(gate())));
        self
    }

    /// Override mode instead of reading from `REPLAY` env var.
    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Set the poll interval for live mode fallback polling.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Set the batch size for loading events from the log.
    pub fn batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Set how often to checkpoint during replay.
    pub fn checkpoint_interval(mut self, interval: usize) -> Self {
        self.checkpoint_interval = interval;
        self
    }

    /// Optional progress callback for replay mode.
    ///
    /// Called at each checkpoint interval with current progress.
    /// Ignored in live mode.
    pub fn on_progress(mut self, callback: impl Fn(ReplayProgress) + Send + Sync + 'static) -> Self {
        self.progress_callback = Some(Box::new(callback));
        self
    }

    /// Resolve the current version.
    ///
    /// - **Live mode**: returns the promoted `active` position from the pointer.
    /// - **Replay mode**: snapshots `latest_position()` from the event log,
    ///   stages it, and returns it. This is the target version for the replay.
    ///
    /// Call before `run()` to derive the database name:
    ///
    /// ```ignore
    /// let stream = ProjectionStream::new(&log, &pointer);
    /// let version = stream.version().await?;
    /// let neo4j = connect(&format!("neo4j.v{version}"));
    /// stream.run(|event| apply(event)).await?;
    /// ```
    pub async fn version(&self) -> Result<u64> {
        let mode = self.mode.unwrap_or_else(Mode::from_env);
        match mode {
            Mode::Live => Ok(self.pointer.version().await?.unwrap_or(0)),
            Mode::Replay => {
                let target = self.log.latest_position().await?;
                self.pointer.stage(target).await?;
                tracing::info!(target, "replay target version staged");
                Ok(target)
            }
        }
    }

    /// Run the projection stream with per-event callbacks.
    ///
    /// Checks `REPLAY` env var to determine mode (unless overridden with `.mode()`):
    /// - Replay: full replay from position 0, promote on success, exit
    /// - Live: catch up from active pointer, tail indefinitely
    pub async fn run<F, Fut>(self, apply: F) -> Result<()>
    where
        F: Fn(&PersistedEvent) -> Fut + Send + Sync,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mode = self.mode.unwrap_or_else(Mode::from_env);
        match mode {
            Mode::Replay => self.run_replay(&apply).await,
            Mode::Live => self.run_live(&apply).await,
        }
    }

    /// Run the projection stream with batch callbacks.
    ///
    /// Same lifecycle as `run()` but passes the whole batch from `load_from`
    /// to the callback instead of iterating per-event. This lets consumers
    /// batch writes (e.g. Neo4j transactions, bulk inserts).
    ///
    /// Checkpointing happens after each batch callback, not per-event.
    /// The consumer owns atomicity within the batch.
    pub async fn run_batch<F, Fut>(self, apply: F) -> Result<()>
    where
        F: Fn(&[PersistedEvent]) -> Fut + Send + Sync,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mode = self.mode.unwrap_or_else(Mode::from_env);
        match mode {
            Mode::Replay => self.run_batch_replay(&apply).await,
            Mode::Live => self.run_batch_live(&apply).await,
        }
    }

    /// Replay mode: read all events from position 0, stage, promote.
    async fn run_replay<F, Fut>(&self, apply: &F) -> Result<()>
    where
        F: Fn(&PersistedEvent) -> Fut + Send + Sync,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut position = 0u64;
        let mut count = 0usize;
        let target = self.log.latest_position().await?;

        tracing::info!(target, "replay starting from position 0");

        loop {
            let events = self.log.load_from(position, self.batch_size).await?;
            if events.is_empty() {
                break;
            }

            for event in &events {
                // Fail-fast in replay mode — bugs should stop before promotion.
                apply(event).await?;
                position = event.position;
                count += 1;

                if count % self.checkpoint_interval == 0 {
                    self.pointer.stage(position).await?;
                    tracing::info!(position, count, target, "replay progress");

                    if let Some(ref cb) = self.progress_callback {
                        cb(ReplayProgress { processed: count, target, position });
                    }
                }
            }
        }

        self.finish_replay(position, count, target).await
    }

    /// Replay mode with batch callbacks.
    async fn run_batch_replay<F, Fut>(&self, apply: &F) -> Result<()>
    where
        F: Fn(&[PersistedEvent]) -> Fut + Send + Sync,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut position = 0u64;
        let mut count = 0usize;
        let target = self.log.latest_position().await?;

        tracing::info!(target, "replay starting from position 0");

        loop {
            let events = self.log.load_from(position, self.batch_size).await?;
            if events.is_empty() {
                break;
            }

            // Fail-fast in replay mode — bugs should stop before promotion.
            apply(&events).await?;

            count += events.len();
            position = events.last().unwrap().position;

            self.pointer.stage(position).await?;
            tracing::info!(position, count, target, "replay progress");

            if let Some(ref cb) = self.progress_callback {
                cb(ReplayProgress { processed: count, target, position });
            }
        }

        self.finish_replay(position, count, target).await
    }

    /// Shared replay finish: final stage, progress, promotion.
    async fn finish_replay(&self, position: u64, count: usize, target: u64) -> Result<()> {
        self.pointer.stage(position).await?;
        tracing::info!(position, count, target, "replay complete");

        if let Some(ref cb) = self.progress_callback {
            cb(ReplayProgress { processed: count, target, position });
        }

        // Promotion gate.
        if let Some(ref gate) = self.promote_gate {
            if gate().await? {
                let promoted = self.pointer.promote().await?;
                tracing::info!(promoted, "promoted");
            } else {
                tracing::warn!(position, "promotion gate failed, staged only");
                anyhow::bail!("promotion gate failed at position {position}");
            }
        } else {
            // No gate = auto-promote.
            let promoted = self.pointer.promote().await?;
            tracing::info!(promoted, "promoted");
        }

        Ok(())
    }

    /// Live mode: catch up from active pointer, then tail.
    async fn run_live<F, Fut>(&self, apply: &F) -> Result<()>
    where
        F: Fn(&PersistedEvent) -> Fut + Send + Sync,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut position = self.pointer.version().await?.unwrap_or(0);
        tracing::info!(position, "live mode: catching up");

        // Catch up.
        loop {
            let events = self.log.load_from(position, self.batch_size).await?;
            if events.is_empty() {
                break;
            }

            for event in &events {
                // Log and continue in live mode.
                if let Err(e) = apply(event).await {
                    tracing::warn!(
                        position = event.position,
                        error = %e,
                        "projection error, skipping"
                    );
                }
                position = event.position;
            }
            self.pointer.save(position).await?;
        }

        tracing::info!(position, "caught up, tailing");

        // Build the fallback poll source.
        let poll_source = PollTailSource::new(self.poll_interval);

        // Tail loop.
        loop {
            match &self.tail_source {
                Some(source) => {
                    tokio::select! {
                        result = source.wait() => {
                            if let Err(e) = result {
                                tracing::warn!(error = %e, "tail source error, falling back to poll");
                                poll_source.wait().await?;
                            }
                        }
                        _ = tokio::time::sleep(self.poll_interval) => {}
                    }
                }
                None => {
                    poll_source.wait().await?;
                }
            }

            let events = self.log.load_from(position, self.batch_size).await?;
            for event in &events {
                if let Err(e) = apply(event).await {
                    tracing::warn!(
                        position = event.position,
                        error = %e,
                        "projection error, skipping"
                    );
                }
                position = event.position;
            }
            if !events.is_empty() {
                self.pointer.save(position).await?;
            }
        }
    }

    /// Live mode with batch callbacks.
    async fn run_batch_live<F, Fut>(&self, apply: &F) -> Result<()>
    where
        F: Fn(&[PersistedEvent]) -> Fut + Send + Sync,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut position = self.pointer.version().await?.unwrap_or(0);
        tracing::info!(position, "live mode: catching up");

        // Catch up.
        loop {
            let events = self.log.load_from(position, self.batch_size).await?;
            if events.is_empty() {
                break;
            }

            if let Err(e) = apply(&events).await {
                tracing::warn!(
                    error = %e,
                    "batch projection error, skipping"
                );
            }
            position = events.last().unwrap().position;
            self.pointer.save(position).await?;
        }

        tracing::info!(position, "caught up, tailing");

        // Build the fallback poll source.
        let poll_source = PollTailSource::new(self.poll_interval);

        // Tail loop.
        loop {
            match &self.tail_source {
                Some(source) => {
                    tokio::select! {
                        result = source.wait() => {
                            if let Err(e) = result {
                                tracing::warn!(error = %e, "tail source error, falling back to poll");
                                poll_source.wait().await?;
                            }
                        }
                        _ = tokio::time::sleep(self.poll_interval) => {}
                    }
                }
                None => {
                    poll_source.wait().await?;
                }
            }

            let events = self.log.load_from(position, self.batch_size).await?;
            if !events.is_empty() {
                if let Err(e) = apply(&events).await {
                    tracing::warn!(
                        error = %e,
                        "batch projection error, skipping"
                    );
                }
                position = events.last().unwrap().position;
                self.pointer.save(position).await?;
            }
        }
    }
}
