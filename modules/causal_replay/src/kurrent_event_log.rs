//! KurrentDB-backed [`EventLogBackend`] implementation.
//!
//! The v0.4 trait shapes (`Event::CATEGORY`, `Event::subject_id`,
//! `Engine::emit`, etc.) were designed against KurrentDB's primitives.
//! This module is the actual implementation; the trait alignment lives
//! in `docs/plans/2026-05-11-v0.4-api-sharpening-plan.md`.
//!
//! ## Locked design decisions (see
//! `docs/plans/2026-05-14-feat-kurrent-eventlog-backend-plan.md`)
//!
//! - **Q1 idempotency.** CAS path (`append_to_stream`) uses
//!   `StreamState::StreamRevision`; on `WrongExpectedVersion`, the
//!   backend reads the conflict slice and classifies it with the
//!   shared [`crate::reconcile`] helper (verifying EVERY batch
//!   event_id, in order — not just the tail): a clean redelivery
//!   returns the existing WriteResult; a real OCC collision surfaces
//!   a typed `ConflictError` (matching the PG backend's shape); a
//!   partial overlap — some-but-not-all ids present — fails loudly.
//!   Under expected version, Kurrent's EventId dedup is a strong
//!   guarantee.
//!   `Any` appends do NOT rely on Kurrent's best-effort EventId dedup
//!   (which compares only against events at the stream head, so a retry
//!   interleaved with a foreign append could duplicate). Instead they
//!   go through `append_any_idempotent`: a scan-then-CAS that reads the
//!   stream tail, classifies the batch with the shared `reconcile`
//!   helper, and appends at the observed head via CAS (re-scanning if a
//!   racing writer moved it). This makes `Any` honor the trait's
//!   "idempotent on event_id" contract absolutely — the same guarantee
//!   Postgres' `UNIQUE(event_id)` and MemoryStore provide. (Closes the
//!   former best-effort gap; see the 2026-06-10 audit remediation, B3.)
//! - **Q2 stream naming.** Every event lands in `{category}-{subject_id}`
//!   (`Event::SUBJECT` + `Event::subject_id`). `category` and `subject_id`
//!   are recovered on read by parsing the stream name (the trailing 36
//!   chars are the canonical UUID) — no metadata round-trip needed.
//! - **Q3 metadata.** Mapped to Kurrent's `custom_metadata` slot.
//!   System keys (`$correlationId`, `$causationId`) use Kurrent's
//!   `$`-prefix convention — the domain's `workflow_id` maps to Kurrent's
//!   `$correlationId`. The `$by_correlation_id` system projection reads
//!   `$correlationId` (when configured + projections are running) and uses
//!   `$causationId` to build the causation tree. There is NO
//!   `$by_causation_id` system projection — the five built-ins are
//!   `$by_category`, `$by_event_type`, `$by_correlation_id`,
//!   `$stream_by_category`, `$streams`. The one causal-specific key
//!   (`_persistent`) keeps the `_` prefix to mark "framework-internal";
//!   it's stamped on write and stripped on read.
//! - **Q4 client.** Uses the official `kurrentdb` crate.
//!
//! ## What this module does NOT provide
//!
//! - No `CheckpointStore` / `ReactorCheckpoint` on Kurrent — keep using
//!   the PG backends. Kurrent is an event store, not a job queue
//!   (roadmap Option B: hybrid).
//! - No `SnapshotStore` on Kurrent — application-level concern;
//!   `PgSnapshotStore` covers it.
//! - No catch-up subscriptions / cross-node sync — Phase 3+ work.

#[cfg(feature = "kurrent")]
mod kurrent {
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use kurrentdb::{
        AppendToStreamOptions, Client, ClientSettings, CurrentRevision,
        EventData as KurrentEventData, Position, ReadAllOptions, ReadStreamOptions,
        RecordedEvent as KurrentRecordedEvent, StreamPosition, StreamState,
        SubscriptionFilter,
    };
    use serde_json::{Map, Value};
    use uuid::Uuid;

    use causal::types::{
        WriteResult, EventData, LogCursor, RecordedEvent, StreamRevision,
    };
    use causal::EventLogBackend;

    use crate::reconcile::{reconcile, Reconciliation};

    /// KurrentDB-backed event log.
    ///
    /// Construct with a connection string (`kurrentdb://...`) or by
    /// passing a pre-built `Client`. See module docs for design
    /// decisions.
    pub struct KurrentEventLogBackend {
        client: Client,
        /// Optional authoritative global `event_id` index (A2). When set,
        /// `Any` appends consult it first, so a redelivery is recognized even
        /// when the original output is buried past the tail-window scan —
        /// required for the 0.19 decision-record completion path to be safe on
        /// Kurrent. When `None`, `Any` falls back to the window-only scan
        /// (legacy behavior; safe only within the window).
        event_id_registry:
            Option<std::sync::Arc<dyn causal::event_id_registry::EventIdRegistry>>,
    }

    impl KurrentEventLogBackend {
        /// Build from a connection string.
        ///
        /// Format: `kurrentdb://[user:pass@]host:port[?option=value&...]`
        /// (the legacy `esdb://` scheme is still accepted as a synonym).
        /// See <https://docs.kurrent.io/clients/grpc/#connection-string>.
        pub fn connect(connection_string: &str) -> Result<Self> {
            let settings: ClientSettings = connection_string
                .parse()
                .map_err(|e| anyhow!("invalid Kurrent connection string: {e}"))?;
            let client = Client::new(settings)
                .map_err(|e| anyhow!("kurrentdb client construction failed: {e}"))?;
            Ok(Self { client, event_id_registry: None })
        }

        /// Build from an already-constructed `kurrentdb::Client`.
        /// Useful for tests that share one client across fixtures.
        pub fn from_client(client: Client) -> Self {
            Self { client, event_id_registry: None }
        }

        /// Attach the authoritative global `event_id` registry (A2). Required
        /// for the decision-record completion path to be safe on Kurrent —
        /// without it, a redelivery whose original output is deeper than the
        /// tail-window scan re-appends a duplicate. The canonical impl is
        /// `PgEventIdRegistry`.
        pub fn with_event_id_registry(
            mut self,
            registry: std::sync::Arc<dyn causal::event_id_registry::EventIdRegistry>,
        ) -> Self {
            self.event_id_registry = Some(registry);
            self
        }

        /// Register a just-appended (or window-recognized) batch in the
        /// global registry, all ids sharing the batch `WriteResult` (A2).
        /// No-op when no registry is attached. Registration errors propagate
        /// so the caller retries: the append is already durable, so a retry
        /// re-recognizes it (window or registry) and re-registers idempotently.
        async fn register_batch(
            &self,
            batch_ids: &[Uuid],
            result: WriteResult,
        ) -> Result<()> {
            if let Some(reg) = &self.event_id_registry {
                let entries: Vec<causal::event_id_registry::EventIdEntry> = batch_ids
                    .iter()
                    .map(|id| causal::event_id_registry::EventIdEntry {
                        event_id: *id,
                        stream_position: result.position,
                        stream_revision: result.revision,
                    })
                    .collect();
                reg.register(&entries).await?;
            }
            Ok(())
        }

        /// Idempotent `Any` append: scan-then-CAS so a duplicate cannot
        /// slip through Kurrent's best-effort EventId dedup.
        ///
        /// Each attempt reads the stream tail, classifies the batch with
        /// the shared [`reconcile`] helper, and — when the batch is
        /// absent — appends at the *observed head* via CAS. A concurrent
        /// writer that moved the head (e.g. a blue/green twin redelivering
        /// the same output, or an unrelated append) trips
        /// `WrongExpectedVersion`; we re-scan. That makes the dedup scan
        /// race-free: a redelivery is recognized (its ids are now in the
        /// window → original `WriteResult`), and a genuine append at a
        /// moved head simply retries at the new head. Bounded so a
        /// pathologically hot stream surfaces a loud error rather than
        /// spinning forever.
        async fn append_any_idempotent(
            &self,
            stream: &str,
            batch_ids: &[Uuid],
            events: &[EventData],
        ) -> Result<WriteResult> {
            // A2: consult the authoritative global registry FIRST. It is
            // unbounded, so it recognizes a redelivery even when the original
            // output is buried far past the tail-window scan below (the exact
            // case the decision-record completion path hits). Absent ⇒ fall
            // through to the window scan + append, which registers on success.
            if let Some(reg) = &self.event_id_registry {
                use causal::event_id_registry::{classify_batch, BatchPresence};
                match classify_batch(reg.as_ref(), batch_ids).await? {
                    BatchPresence::Redelivery { last } => {
                        return Ok(WriteResult {
                            position: last.stream_position,
                            revision: last.stream_revision,
                        });
                    }
                    BatchPresence::PartialOverlap => {
                        return Err(anyhow!(
                            "append_to_stream on {stream}: batch partially overlaps an \
                             earlier append (per the global event_id registry) — \
                             event_ids must be all-new or all-already-persisted"
                        ));
                    }
                    BatchPresence::Absent => {}
                }
            }

            // Window must cover any plausible interleaving of foreign
            // events between the original append and this redelivery.
            let window_size = (batch_ids.len() * 4).max(64);
            for _attempt in 0..16 {
                let (head, window) = read_tail_window(&self.client, stream, window_size).await?;
                let window_ids: Vec<Uuid> = window.iter().map(|w| w.id).collect();
                match reconcile(batch_ids, &window_ids) {
                    Reconciliation::Redelivery => {
                        ensure_redelivery_identical(events, &window)?;
                        let last = batch_ids.last().expect("non-empty batch");
                        let result = window
                            .iter()
                            .find(|w| &w.id == last)
                            .expect("Redelivery ⇒ every batch id is in the window")
                            .result;
                        // Heal the crash-before-register case: a redelivery
                        // the window recognized but the registry missed.
                        self.register_batch(batch_ids, result).await?;
                        return Ok(result);
                    }
                    Reconciliation::PartialOverlap => {
                        return Err(anyhow!(
                            "append_to_stream on {stream}: batch partially overlaps an \
                             earlier append — an event_id already exists but the full \
                             batch does not (event_ids must be all-new or \
                             all-already-persisted)"
                        ));
                    }
                    Reconciliation::Conflict => {
                        // Not present — append at the observed head. CAS so
                        // a racing writer is caught (→ re-scan) instead of
                        // producing a duplicate.
                        let expected = match head {
                            Some(r) => StreamState::StreamRevision(r),
                            None => StreamState::NoStream,
                        };
                        let event_data = events
                            .iter()
                            .map(build_event_data)
                            .collect::<Result<Vec<_>>>()?;
                        let options = AppendToStreamOptions::default().stream_state(expected);
                        match self.client.append_to_stream(stream, &options, event_data).await {
                            Ok(write) => {
                                let wr = WriteResult {
                                    position: LogCursor::from_raw(write.position.commit),
                                    revision: StreamRevision::from_raw(
                                        write.next_expected_version,
                                    ),
                                };
                                // A2: record the batch so a future redelivery
                                // deeper than the window is still recognized.
                                self.register_batch(batch_ids, wr).await?;
                                return Ok(wr);
                            }
                            // Head moved between scan and append — re-scan.
                            Err(kurrentdb::Error::WrongExpectedVersion { .. }) => continue,
                            Err(e) => {
                                return Err(anyhow!("kurrent Any append failed: {e}"))
                            }
                        }
                    }
                }
            }
            Err(anyhow!(
                "append_to_stream on {stream}: idempotent Any append did not converge \
                 after 16 attempts (stream under extreme write contention)"
            ))
        }
    }

    #[async_trait]
    impl EventLogBackend for KurrentEventLogBackend {
        async fn append_to_stream(
            &self,
            category: &str,
            subject_id: Uuid,
            expected: causal::types::StreamState,
            events: Vec<EventData>,
        ) -> Result<WriteResult> {
            use causal::types::StreamState as CausalStreamState;
            // Per Q2 invariant: category must not contain '-'.
            debug_assert!(
                !category.contains('-'),
                "category '{category}' contains '-'; conflicts with \
                 Kurrent's '{{category}}-{{id}}' stream naming convention",
            );
            let stream = format!("{}-{}", category, subject_id);
            if events.is_empty() {
                anyhow::bail!("append_to_stream: events must be non-empty");
            }
            // Batch ids in batch order — the reconcile helper verifies
            // ALL of them on the conflict path, not just the tail.
            let batch_ids: Vec<Uuid> = events.iter().map(|e| e.event_id).collect();
            // Kurrent commits the whole iterator as one atomic batch.
            let event_data = events
                .iter()
                .map(build_event_data)
                .collect::<Result<Vec<_>>>()?;

            // `Any` would map to Kurrent's best-effort EventId dedup,
            // which the module docs (Q?) flag as unreliable: a duplicate
            // can slip through when a redelivery races a foreign write
            // onto the same stream. The trait contract requires
            // idempotency on `event_id` (Postgres' UNIQUE constraint and
            // MemoryStore both honor it absolutely). Make Kurrent honor
            // it too via an explicit scan-then-CAS, reusing the same
            // `reconcile` machinery the conflict path uses.
            if matches!(expected, CausalStreamState::Any) {
                return self
                    .append_any_idempotent(&stream, &batch_ids, &events)
                    .await;
            }

            // causal::StreamState and kurrentdb::StreamState are
            // structurally identical; map variant-for-variant.
            let kurrent_state = match expected {
                CausalStreamState::Any => StreamState::Any,
                CausalStreamState::NoStream => StreamState::NoStream,
                CausalStreamState::StreamExists => StreamState::StreamExists,
                CausalStreamState::StreamRevision(r) => StreamState::StreamRevision(r),
            };
            let options = AppendToStreamOptions::default()
                .stream_state(kurrent_state);

            match self
                .client
                .append_to_stream(stream.as_str(), &options, event_data)
                .await
            {
                Ok(write) => Ok(WriteResult {
                    position: LogCursor::from_raw(write.position.commit),
                    // `next_expected_version` is Kurrent's name for
                    // "revision of the just-written event" — 0-indexed,
                    // matching causal::StreamRevision directly.
                    revision: StreamRevision::from_raw(write.next_expected_version),
                }),
                Err(kurrentdb::Error::WrongExpectedVersion { current, .. }) => {
                    let current_rev = match current {
                        CurrentRevision::Current(n) => Some(n),
                        CurrentRevision::NoStream => None,
                    };

                    // Reconciliation: read the conflict window (every
                    // event after the caller's expected revision) and
                    // let the shared `reconcile` helper classify the
                    // append — Redelivery / Conflict / PartialOverlap.
                    // A window only exists when the stream moved PAST
                    // the expectation; an expectation AHEAD of the head
                    // (or a missing stream) is a plain conflict.
                    let window = match (expected, current_rev) {
                        (CausalStreamState::StreamRevision(want), Some(c)) if c > want => {
                            Some(read_conflict_window(
                                &self.client,
                                &stream,
                                StreamPosition::Position(want + 1),
                                (c - want) as usize,
                            ).await?)
                        }
                        (CausalStreamState::NoStream, Some(c)) => {
                            Some(read_conflict_window(
                                &self.client,
                                &stream,
                                StreamPosition::Start,
                                (c as usize) + 1,
                            ).await?)
                        }
                        _ => None,
                    };
                    if let Some(window) = window {
                        let window_ids: Vec<Uuid> =
                            window.iter().map(|w| w.id).collect();
                        match reconcile(&batch_ids, &window_ids) {
                            Reconciliation::Redelivery => {
                                // The whole batch already landed on an
                                // earlier attempt — verify it's byte-
                                // identical, then return the ORIGINAL
                                // WriteResult (last batch event's
                                // coordinates), never an error.
                                ensure_redelivery_identical(&events, &window)?;
                                let last = batch_ids.last().expect("non-empty");
                                let result = window
                                    .iter()
                                    .find(|w| &w.id == last)
                                    .expect("Redelivery ⇒ every batch id is in the window")
                                    .result;
                                return Ok(result);
                            }
                            Reconciliation::PartialOverlap => {
                                return Err(anyhow!(
                                    "append_to_stream on {stream}: batch partially \
                                     overlaps an earlier append — an event_id \
                                     already exists but the full batch does not \
                                     (event_ids must be all-new or \
                                     all-already-persisted)"
                                ));
                            }
                            Reconciliation::Conflict => {} // fall through
                        }
                    }
                    // Typed ConflictError so Engine::append can downcast
                    // + retry (not a bare string).
                    Err(anyhow::Error::new(causal::event_log::ConflictError {
                        expected,
                        current: current_rev.map(StreamRevision::from_raw),
                    }))
                }
                Err(e) => Err(anyhow!("kurrent append_to_stream failed: {e}")),
            }
        }

        async fn read_all(
            &self,
            after: LogCursor,
            limit: usize,
        ) -> Result<Vec<RecordedEvent>> {
            // Kurrent's $all positions are 2D (commit/prepare); our
            // cursor is the commit position. Reading "after commit X"
            // means starting from X and filtering `commit > X`
            // post-fetch (the kurrent client doesn't expose strict
            // exclusive semantics on Position; we get them by skipping
            // any boundary event ourselves).
            let opts = ReadAllOptions::default()
                .forwards()
                .position(StreamPosition::Position(Position {
                    commit: after.raw(),
                    prepare: after.raw(),
                }))
                .max_count(limit + 1)
                .filter(SubscriptionFilter::on_event_type().exclude_system_events());

            let mut stream = self
                .client
                .read_all(&opts)
                .await
                .map_err(|e| anyhow!("kurrent read_all failed: {e}"))?;

            let mut out = Vec::with_capacity(limit);
            while let Some(resolved) = stream
                .next()
                .await
                .map_err(|e| anyhow!("kurrent read_all next failed: {e}"))?
            {
                let recorded = resolved.get_original_event();
                if recorded.position.commit <= after.raw() {
                    continue;
                }
                out.push(recorded_to_persisted(recorded)?);
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        }

        async fn read_stream(
            &self,
            category: &str,
            subject_id: Uuid,
            after: Option<StreamRevision>,
        ) -> Result<Vec<RecordedEvent>> {
            let stream_name = format!("{}-{}", category, subject_id);
            // causal::StreamRevision is 0-indexed, matching Kurrent
            // exactly. To return events with revision > r, start
            // reading at position r + 1.
            let position = match after {
                Some(r) => StreamPosition::Position(r.raw() + 1),
                None => StreamPosition::Start,
            };
            // Kurrent's read_stream requires max_count via the options
            // builder; pass usize::MAX equivalent by using a large
            // window. Callers paginate at a higher level.
            let opts = ReadStreamOptions::default()
                .forwards()
                .position(position)
                .max_count(usize::MAX);

            let mut stream = match self
                .client
                .read_stream(stream_name.as_str(), &opts)
                .await
            {
                Ok(s) => s,
                Err(kurrentdb::Error::ResourceNotFound) => return Ok(Vec::new()),
                Err(e) => {
                    return Err(anyhow!(
                        "kurrent read_stream '{stream_name}' failed: {e}"
                    ));
                }
            };

            let mut out = Vec::new();
            loop {
                match stream.next().await {
                    Ok(Some(resolved)) => {
                        let recorded = resolved.get_original_event();
                        out.push(recorded_to_persisted(recorded)?);
                    }
                    Ok(None) => break,
                    // On a real KurrentDB, a missing stream isn't reported on
                    // the initial `read_stream` call — it surfaces here, on the
                    // first `next()`. Treat it as an empty stream (contract:
                    // missing stream → empty Vec, never an error).
                    Err(kurrentdb::Error::ResourceNotFound) => break,
                    Err(e) => {
                        return Err(anyhow!("kurrent read_stream next failed: {e}"));
                    }
                }
            }
            Ok(out)
        }

        async fn latest_position(&self) -> Result<LogCursor> {
            let opts = ReadAllOptions::default()
                .backwards()
                .position(StreamPosition::End)
                .max_count(1)
                .filter(SubscriptionFilter::on_event_type().exclude_system_events());

            let mut stream = self
                .client
                .read_all(&opts)
                .await
                .map_err(|e| anyhow!("kurrent latest_position read failed: {e}"))?;

            match stream
                .next()
                .await
                .map_err(|e| anyhow!("kurrent latest_position next failed: {e}"))?
            {
                Some(resolved) => {
                    let rec = resolved.get_original_event();
                    Ok(LogCursor::from_raw(rec.position.commit))
                }
                None => Ok(LogCursor::ZERO),
            }
        }
    }

    // ──────────────────────────────────────────────────────────────
    // Helpers
    // ──────────────────────────────────────────────────────────────

    /// Build a `kurrentdb::EventData` from causal's `EventData`. Stamps
    /// the causal-reserved metadata keys.
    fn build_event_data(event: &EventData) -> Result<KurrentEventData> {
        let data = KurrentEventData::json(&event.event_type, &event.payload)
            .map_err(|e| anyhow!("event payload not JSON-serializable: {e}"))?
            .id(event.event_id);

        let metadata = build_metadata(event);
        let data = data
            .metadata_as_json(&metadata)
            .map_err(|e| anyhow!("metadata not JSON-serializable: {e}"))?;
        Ok(data)
    }

    fn build_metadata(event: &EventData) -> Map<String, Value> {
        let mut m = event.metadata.clone();
        // KurrentDB convention: system metadata keys are `$`-prefixed
        // camelCase. The domain's `workflow_id` maps to Kurrent's
        // `$correlationId`, which the `$by_correlation_id` system projection
        // reads (once configured + projections running); `$causationId`
        // builds the causation tree. There is no `$by_causation_id`
        // projection. Using these exact names is the difference between the
        // native projection working and silently returning nothing.
        m.insert(
            "$correlationId".to_string(),
            Value::String(event.workflow_id.to_string()),
        );
        if let Some(causation) = event.causation_id {
            m.insert(
                "$causationId".to_string(),
                Value::String(causation.to_string()),
            );
        }
        // causal-specific reserved key (no Kurrent counterpart): keep `_`
        // prefix to mark "framework-internal." (category/subject_id are
        // recovered from the stream name, so no `_aggregateType` needed.)
        m.insert("_persistent".to_string(), Value::Bool(event.persistent));
        m
    }

    /// Reverse-map a `kurrentdb::RecordedEvent` into causal's `RecordedEvent`.
    fn recorded_to_persisted(rec: &KurrentRecordedEvent) -> Result<RecordedEvent> {
        let metadata_value: Value = if rec.custom_metadata.is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_slice(&rec.custom_metadata)
                .map_err(|e| anyhow!("malformed Kurrent metadata: {e}"))?
        };
        let mut metadata = match metadata_value {
            Value::Object(m) => m,
            _ => Map::new(),
        };

        let workflow_id = metadata
            .remove("$correlationId")
            .and_then(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
            .ok_or_else(|| anyhow!("Kurrent event missing $correlationId"))?;
        let causation_id = metadata
            .remove("$causationId")
            .and_then(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()));
        let persistent = metadata
            .remove("_persistent")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        // category + subject_id are recovered from the Kurrent stream name
        // `{category}-{subject_id}` (subject_id is a canonical 36-char UUID
        // at the end). No `_aggregateType` metadata needed.
        let stream_name = rec.stream_id(); // kurrentdb crate API — its vocabulary, not ours
        let subject_id = stream_name
            .get(stream_name.len().saturating_sub(36)..)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| {
                anyhow!("Kurrent stream name '{stream_name}' does not end in a UUID")
            })?;
        let category = stream_name
            .strip_suffix(&format!("-{subject_id}"))
            .unwrap_or_default()
            .to_string();
        // causal::StreamRevision and Kurrent revision are both 0-indexed.
        let revision = StreamRevision::from_raw(rec.revision);

        let payload: Value = serde_json::from_slice(&rec.data)
            .map_err(|e| anyhow!("malformed Kurrent payload: {e}"))?;

        let created_at: DateTime<Utc> = rec.created;

        Ok(RecordedEvent {
            position: LogCursor::from_raw(rec.position.commit),
            event_id: rec.id,
            causation_id,
            workflow_id,
            event_type: rec.event_type.clone(),
            payload,
            category,
            subject_id,
            revision,
            metadata,
            created_at,
            ephemeral: None,
            persistent,
        })
    }

    /// On `WrongExpectedVersion`, read the conflict window — every
    /// event from `from` for `count` events — and return each event's
    /// `(event_id, WriteResult)` in stream order. The shared
    /// [`reconcile`] helper then classifies the failed append against
    /// the ids; on `Redelivery` the caller picks the last batch id's
    /// `WriteResult` out of this window (the original append's
    /// coordinates).
    ///
    /// For `StreamRevision(want)` the window is `(want, current]`; for
    /// `NoStream` it's the whole stream (the caller saw an `Ok` on a
    /// previous append, Kurrent's EventId cache evicted it, and the
    /// server now reports the stream has events — check if they're
    /// ours).
    /// One event of a dedup/conflict window: its id, original append
    /// coordinates, and the content fields the byte-identical-redelivery
    /// check compares. `content` is `None` when the persisted record
    /// isn't causal-shaped (a foreign writer's event — never a batch
    /// match unless ids collide, in which case the comparison fails
    /// loudly rather than assuming identity).
    struct WindowEvent {
        id:      Uuid,
        result:  WriteResult,
        content: Option<WindowContent>,
    }

    struct WindowContent {
        event_type:     String,
        payload:        Value,
        workflow_id: Option<Uuid>,
        causation_id:   Option<Uuid>,
    }

    fn window_event_from(rec: &KurrentRecordedEvent) -> WindowEvent {
        let content = (|| {
            let payload: Value = serde_json::from_slice(&rec.data).ok()?;
            let metadata: Value = if rec.custom_metadata.is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_slice(&rec.custom_metadata).ok()?
            };
            let workflow_id = metadata
                .get("$correlationId")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            let causation_id = metadata
                .get("$causationId")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            Some(WindowContent {
                event_type: rec.event_type.clone(),
                payload,
                workflow_id,
                causation_id,
            })
        })();
        WindowEvent {
            id: rec.id,
            result: WriteResult {
                position: LogCursor::from_raw(rec.position.commit),
                revision: StreamRevision::from_raw(rec.revision),
            },
            content,
        }
    }

    /// A dedup-hit must be a **byte-identical** redelivery (see the
    /// `EventLogBackend` idempotency contract): a persisted row whose
    /// `payload` / `event_type` / `workflow_id` / `causation_id`
    /// differs from the redelivered batch means the producer re-decided
    /// differently on redelivery — error loudly instead of silently
    /// keeping the old row. `created_at` and `metadata` are exempt
    /// (documented hints that redeliveries re-stamp). Pure; called on
    /// the `Redelivery` reconciliation branches, where every batch id
    /// is known to be present in the window.
    ///
    /// NOTE: placement (`category`/`subject_id`) is intentionally NOT
    /// compared here. Kurrent's dedup is per-stream — this window is the
    /// *target stream's* tail, so a cross-stream `event_id` reuse never
    /// appears in `window` and cannot be detected without a global
    /// `event_id`→stream index. Unlike Memory/Postgres, this backend does
    /// not yet enforce the placement-identity clause of the
    /// `EventLogBackend` contract; see the idempotency-index work.
    fn ensure_redelivery_identical(
        batch: &[EventData],
        window: &[WindowEvent],
    ) -> Result<()> {
        for e in batch {
            let Some(row) = window.iter().find(|w| w.id == e.event_id) else {
                continue; // not a dedup-hit for this id
            };
            let identical = row.content.as_ref().is_some_and(|c| {
                c.payload == e.payload
                    && c.event_type == e.event_type
                    && c.workflow_id == Some(e.workflow_id)
                    && c.causation_id == e.causation_id
            });
            if !identical {
                // Typed so the reactor runner can tell this apart from
                // genuine I/O by downcast (it accepts the persisted row and
                // shouts rather than retrying forever). Mirrors the
                // ConflictError construction elsewhere in this backend.
                return Err(anyhow::Error::new(
                    causal::event_log::DivergentRedelivery {
                        event_id: e.event_id,
                        diff:     "payload/event_type/workflow/causation".to_string(),
                    },
                ));
            }
        }
        Ok(())
    }

    /// Read the last `count` events of a stream — the tail window for
    /// the idempotent `Any` append's dedup scan. Returns the head
    /// revision (`None` if the stream is empty/absent) and the window as
    /// [`WindowEvent`]s in ascending stream order.
    async fn read_tail_window(
        client: &Client,
        stream: &str,
        count: usize,
    ) -> Result<(Option<u64>, Vec<WindowEvent>)> {
        let opts = ReadStreamOptions::default()
            .backwards()
            .position(StreamPosition::End)
            .max_count(count.max(1));
        let mut read = match client.read_stream(stream, &opts).await {
            Ok(s) => s,
            Err(kurrentdb::Error::ResourceNotFound) => return Ok((None, Vec::new())),
            Err(e) => return Err(anyhow!("kurrent tail read failed: {e}")),
        };
        // Backwards read yields highest-revision first.
        let mut window = Vec::new();
        loop {
            let resolved = match read.next().await {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(kurrentdb::Error::ResourceNotFound) => break,
                Err(e) => return Err(anyhow!("kurrent tail next failed: {e}")),
            };
            let rec = resolved.get_original_event();
            window.push(window_event_from(rec));
        }
        let head = window.first().map(|w| w.result.revision.raw());
        window.reverse(); // ascending stream order, as reconcile expects
        Ok((head, window))
    }

    async fn read_conflict_window(
        client: &Client,
        stream: &str,
        from: StreamPosition<u64>,
        count: usize,
    ) -> Result<Vec<WindowEvent>> {
        let opts = ReadStreamOptions::default()
            .forwards()
            .position(from)
            .max_count(count.max(1));
        let mut read = match client.read_stream(stream, &opts).await {
            Ok(s) => s,
            Err(kurrentdb::Error::ResourceNotFound) => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!("kurrent reconcile read failed: {e}")),
        };
        let mut window = Vec::new();
        loop {
            let resolved = match read.next().await {
                Ok(Some(r)) => r,
                Ok(None) => break,
                // Missing stream surfaces on `next()`, not the initial call.
                Err(kurrentdb::Error::ResourceNotFound) => break,
                Err(e) => return Err(anyhow!("kurrent reconcile next failed: {e}")),
            };
            let rec = resolved.get_original_event();
            window.push(window_event_from(rec));
        }
        Ok(window)
    }

    // ──────────────────────────────────────────────────────────────
    // Pure-function tests (no Kurrent connection)
    // ──────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use causal::types::EventData;

        fn mk_event(
            event_type: &str,
            category: Option<&str>,
            subject_id: Option<Uuid>,
        ) -> EventData {
            EventData {
                event_id:        Uuid::new_v4(),
                causation_id:       None,
                workflow_id:  Uuid::new_v4(),
                event_type:      event_type.to_string(),
                payload:         serde_json::json!({}),
                created_at:      Utc::now(),
                category:        category.map(String::from),
                subject_id:       subject_id,
                metadata:        Map::new(),
                ephemeral:       None,
                persistent:      true,
            }
        }

        // The old "causal version vs Kurrent revision" conversion
        // helpers are gone in this release — causal::StreamRevision
        // is now 0-indexed and maps 1:1 to Kurrent's revision (no
        // off-by-one to test). End-to-end version semantics are
        // covered by the conformance suite running against a live
        // Kurrent (see
        // `tests/kurrent_event_log_conformance_test.rs`).

        fn window_row_for(e: &EventData) -> WindowEvent {
            WindowEvent {
                id: e.event_id,
                result: WriteResult {
                    position: LogCursor::from_raw(7),
                    revision: StreamRevision::from_raw(3),
                },
                content: Some(WindowContent {
                    event_type:     e.event_type.clone(),
                    payload:        e.payload.clone(),
                    workflow_id: Some(e.workflow_id),
                    causation_id:   e.causation_id,
                }),
            }
        }

        #[test]
        fn identical_redelivery_passes_divergence_check() {
            let e = mk_event("conformance:c1", None, None);
            let window = vec![window_row_for(&e)];
            ensure_redelivery_identical(&[e], &window)
                .expect("byte-identical redelivery must pass");
        }

        #[test]
        fn divergent_payload_fails_divergence_check() {
            let mut e = mk_event("conformance:c1b", None, None);
            e.payload = serde_json::json!({"decision": "ship"});
            let mut row = window_row_for(&e);
            row.content.as_mut().unwrap().payload =
                serde_json::json!({"decision": "cancel"});
            let err = ensure_redelivery_identical(&[e], &[row])
                .expect_err("divergent payload must be rejected");
            assert!(err.to_string().contains("divergent"), "got: {err:#}");
        }

        #[test]
        fn divergent_correlation_fails_divergence_check() {
            let e = mk_event("conformance:c1b", None, None);
            let mut row = window_row_for(&e);
            row.content.as_mut().unwrap().workflow_id = Some(Uuid::new_v4());
            assert!(ensure_redelivery_identical(&[e], &[row]).is_err());
        }

        #[test]
        fn unparseable_persisted_row_fails_divergence_check() {
            // Same event_id but the persisted record isn't causal-shaped:
            // identity can't be verified, so fail loudly rather than
            // assume.
            let e = mk_event("conformance:c1b", None, None);
            let mut row = window_row_for(&e);
            row.content = None;
            assert!(ensure_redelivery_identical(&[e], &[row]).is_err());
        }

        #[test]
        fn foreign_window_rows_are_ignored_by_divergence_check() {
            // A window event with a DIFFERENT id is a foreign neighbor,
            // not a dedup-hit — never compared.
            let e = mk_event("conformance:c1", None, None);
            let mut foreign = window_row_for(&e);
            foreign.id = Uuid::new_v4();
            foreign.content = None;
            ensure_redelivery_identical(&[e], &[foreign])
                .expect("foreign rows must not trip the check");
        }

        #[test]
        fn build_metadata_stamps_reserved_keys() {
            let parent = Uuid::new_v4();
            let workflow = Uuid::new_v4();
            let mut e = mk_event("lifecycle:run", Some("lifecycle"), Some(Uuid::new_v4()));
            e.causation_id = Some(parent);
            e.workflow_id = workflow;
            e.metadata.insert("_run_id".into(), Value::String("r-1".into()));

            let m = build_metadata(&e);
            assert_eq!(
                m.get("$correlationId").and_then(Value::as_str),
                Some(workflow.to_string().as_str()),
                "Kurrent convention: $correlationId, not _workflow_id"
            );
            assert_eq!(
                m.get("$causationId").and_then(Value::as_str),
                Some(parent.to_string().as_str()),
                "Kurrent convention: $causationId, not _parent_id"
            );
            assert_eq!(m.get("_persistent").and_then(Value::as_bool), Some(true));
            assert!(
                !m.contains_key("_aggregateType"),
                "category is recovered from the stream name, not metadata",
            );
            // User metadata preserved.
            assert_eq!(
                m.get("_run_id").and_then(Value::as_str),
                Some("r-1")
            );
        }
    }
}

#[cfg(feature = "kurrent")]
pub use kurrent::KurrentEventLogBackend;
