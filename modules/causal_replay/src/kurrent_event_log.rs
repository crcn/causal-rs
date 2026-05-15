//! KurrentDB-backed [`EventLogBackend`] implementation.
//!
//! The v0.4 trait shapes (`Event::CATEGORY`, `Event::stream_id`,
//! `Engine::emit`, etc.) were designed against KurrentDB's primitives.
//! This module is the actual implementation; the trait alignment lives
//! in `docs/plans/2026-05-11-v0.4-api-sharpening-plan.md`.
//!
//! ## Locked design decisions (see
//! `docs/plans/2026-05-14-feat-kurrent-eventlog-backend-plan.md`)
//!
//! - **Q1 idempotency.** CAS path (`append_to_stream`) uses
//!   `StreamState::StreamRevision`; on `WrongExpectedVersion`, the
//!   backend reads the conflict slice to distinguish a true duplicate
//!   (return the existing WriteResult) from a real OCC collision
//!   (surface `anyhow::Error` whose message includes the current
//!   stream version, matching the PG backend's shape). Non-CAS
//!   `append` uses `StreamState::Any` + `EventData::id(event_id)` —
//!   Kurrent's ~1-min EventId cache dedups within the window;
//!   post-window retries can produce duplicates. **Documented gap.**
//! - **Q2 stream naming.** Aggregate events land in
//!   `{aggregate_type}-{aggregate_id}`. Non-aggregate events land in
//!   `{category}-_global` where category is extracted from
//!   `event_type` prefix. `_global` is not a valid UUID so it can't
//!   collide with a real aggregate id.
//! - **Q3 metadata.** Mapped to Kurrent's `custom_metadata` slot.
//!   System keys (`$correlationId`, `$causationId`) use Kurrent's
//!   `$`-prefix convention so server-side projections
//!   (`$by_correlation_id`, `$by_causation_id`) read them natively.
//!   Causal-specific keys (`_persistent`, `_aggregateType`) keep
//!   the `_` prefix to mark "framework-internal." All are stamped
//!   on write and stripped on read.
//! - **Q4 client.** Uses the official `kurrentdb` crate.
//!
//! ## What this module does NOT provide
//!
//! - No `CheckpointStore` / `ReactorOutbox` on Kurrent — keep using
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

    /// KurrentDB-backed event log.
    ///
    /// Construct with a connection string (`esdb://...`) or by passing
    /// a pre-built `Client`. See module docs for design decisions.
    pub struct KurrentEventLogBackend {
        client: Client,
    }

    impl KurrentEventLogBackend {
        /// Build from a connection string.
        ///
        /// Format: `esdb://[user:pass@]host:port[?option=value&...]`.
        /// See <https://docs.kurrent.io/clients/grpc/#connection-string>.
        pub fn connect(connection_string: &str) -> Result<Self> {
            let settings: ClientSettings = connection_string
                .parse()
                .map_err(|e| anyhow!("invalid Kurrent connection string: {e}"))?;
            let client = Client::new(settings)
                .map_err(|e| anyhow!("kurrentdb client construction failed: {e}"))?;
            Ok(Self { client })
        }

        /// Build from an already-constructed `kurrentdb::Client`.
        /// Useful for tests that share one client across fixtures.
        pub fn from_client(client: Client) -> Self {
            Self { client }
        }
    }

    #[async_trait]
    impl EventLogBackend for KurrentEventLogBackend {
        async fn append(&self, event: EventData) -> Result<WriteResult> {
            // Non-CAS append. `StreamState::Any` permits writes
            // regardless of current revision; `EventData::id(...)`
            // gives EventId-based dedup inside Kurrent's ~1-min cache.
            //
            // Post-cache retries can produce duplicates — documented
            // on this method. Hot-path callers (`Engine::emit` retry)
            // finish inside the cache window.
            let stream = stream_name(&event);
            let event_data = build_event_data(&event)?;
            let options = AppendToStreamOptions::default()
                .stream_state(StreamState::Any);

            let write = self
                .client
                .append_to_stream(stream.as_str(), &options, event_data)
                .await
                .map_err(|e| anyhow!("kurrent append failed for stream '{stream}': {e}"))?;

            Ok(WriteResult {
                position: LogCursor::from_raw(write.position.commit),
                // append() is the non-aggregate path — surface no
                // stream revision (matches the PG backend's behavior).
                revision: None,
            })
        }

        async fn append_to_stream(
            &self,
            aggregate_type: &str,
            aggregate_id: Uuid,
            expected: causal::types::StreamState,
            event: EventData,
        ) -> Result<WriteResult> {
            use causal::types::StreamState as CausalStreamState;
            // Per Q2 invariant: aggregate_type must not contain '-'.
            debug_assert!(
                !aggregate_type.contains('-'),
                "aggregate_type '{aggregate_type}' contains '-'; conflicts with \
                 Kurrent's '{{category}}-{{id}}' stream naming convention",
            );
            let stream = format!("{}-{}", aggregate_type, aggregate_id);
            let event_data = build_event_data(&event)?;

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
                    revision: Some(StreamRevision::from_raw(
                        write.next_expected_version,
                    )),
                }),
                Err(kurrentdb::Error::WrongExpectedVersion { current, .. }) => {
                    let event_id = event.event_id;
                    let current_rev = match current {
                        CurrentRevision::Current(n) => Some(n),
                        CurrentRevision::NoStream => None,
                    };

                    // Reconciliation: scan the conflict window for
                    // our event_id (idempotent-retry path).
                    match (expected, current_rev) {
                        (CausalStreamState::StreamRevision(want), Some(c)) if c > want => {
                            if let Some(found) = find_event_in_range(
                                &self.client, &stream, want, c, event_id,
                            ).await?
                            {
                                return Ok(found);
                            }
                        }
                        (CausalStreamState::NoStream, Some(c)) => {
                            if let Some(found) = find_event_in_range_from_start(
                                &self.client, &stream, c, event_id,
                            ).await?
                            {
                                return Ok(found);
                            }
                        }
                        _ => {}
                    }
                    Err(anyhow!(
                        "OCC conflict on '{stream}': expected {}, current revision is {:?}",
                        expected,
                        current_rev,
                    ))
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
            aggregate_type: &str,
            aggregate_id: Uuid,
            after: Option<StreamRevision>,
        ) -> Result<Vec<RecordedEvent>> {
            let stream_name = format!("{}-{}", aggregate_type, aggregate_id);
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
            while let Some(resolved) = stream
                .next()
                .await
                .map_err(|e| anyhow!("kurrent read_stream next failed: {e}"))?
            {
                let recorded = resolved.get_original_event();
                out.push(recorded_to_persisted(recorded)?);
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

    /// Compose the target stream name for a `EventData`.
    ///
    /// - Aggregate events: `{aggregate_type}-{aggregate_id}` (matches
    ///   Kurrent's `$by_category` convention).
    /// - Non-aggregate events: `{category}-_global` where category is
    ///   extracted from `event_type` prefix. Falls back to
    ///   `"causal-_global"` for legacy events with no colon.
    fn stream_name(event: &EventData) -> String {
        match (event.aggregate_type.as_deref(), event.aggregate_id) {
            (Some(t), Some(id)) => {
                debug_assert!(
                    !t.contains('-'),
                    "aggregate_type '{t}' contains '-'; conflicts with \
                     Kurrent's stream naming convention",
                );
                format!("{}-{}", t, id)
            }
            _ => {
                let category = event
                    .event_type
                    .split(':')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("causal");
                format!("{}-_global", category)
            }
        }
    }

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
        // camelCase. `$correlationId` and `$causationId` are what
        // Kurrent's server-side projections (`$by_correlation_id`,
        // `$by_causation_id`) read. Using these specific names is
        // the difference between native projections working and
        // silently returning nothing.
        m.insert(
            "$correlationId".to_string(),
            Value::String(event.correlation_id.to_string()),
        );
        if let Some(causation) = event.causation_id {
            m.insert(
                "$causationId".to_string(),
                Value::String(causation.to_string()),
            );
        }
        // causal-specific reserved keys (no Kurrent counterpart):
        // keep `_` prefix to mark "framework-internal."
        m.insert("_persistent".to_string(), Value::Bool(event.persistent));
        if let Some(ref agg_t) = event.aggregate_type {
            m.insert(
                "_aggregateType".to_string(),
                Value::String(agg_t.clone()),
            );
        }
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

        let correlation_id = metadata
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
        let aggregate_type = metadata
            .remove("_aggregateType")
            .and_then(|v| v.as_str().map(String::from));
        // Parse aggregate_id from stream_id when aggregate_type is set:
        // stream "{type}-{uuid}" → uuid. For non-aggregate events
        // (stream "{cat}-_global"), aggregate_id stays None.
        let aggregate_id = aggregate_type.as_ref().and_then(|t| {
            let prefix = format!("{}-", t);
            rec.stream_id()
                .strip_prefix(&prefix)
                .and_then(|rest| Uuid::parse_str(rest).ok())
        });
        // causal::StreamRevision and Kurrent revision are both
        // 0-indexed; identity mapping.
        let revision = aggregate_type
            .as_ref()
            .map(|_| StreamRevision::from_raw(rec.revision));

        let payload: Value = serde_json::from_slice(&rec.data)
            .map_err(|e| anyhow!("malformed Kurrent payload: {e}"))?;

        let created_at: DateTime<Utc> = rec.created;

        Ok(RecordedEvent {
            position: LogCursor::from_raw(rec.position.commit),
            event_id: rec.id,
            causation_id,
            correlation_id,
            event_type: rec.event_type.clone(),
            payload,
            aggregate_type,
            aggregate_id,
            revision,
            metadata,
            created_at,
            ephemeral: None,
            persistent,
        })
    }

    /// On `WrongExpectedVersion` where the caller expected
    /// `StreamRevision(want)`, scan `(want, current_rev]` for
    /// `event_id` — the idempotent-retry path.
    async fn find_event_in_range(
        client: &Client,
        stream: &str,
        want: u64,
        current_rev: u64,
        event_id: Uuid,
    ) -> Result<Option<WriteResult>> {
        let max = (current_rev - want) as usize;
        let opts = ReadStreamOptions::default()
            .forwards()
            .position(StreamPosition::Position(want + 1))
            .max_count(max.max(1));
        run_reconcile_scan(client, stream, opts, event_id).await
    }

    /// On `WrongExpectedVersion` where the caller expected
    /// `NoStream`, scan the entire stream from the start for
    /// `event_id`. The caller saw an `Ok` on a previous append; the
    /// Kurrent cache evicted the EventId; Kurrent now reports the
    /// stream has events — check whether one of them is ours.
    async fn find_event_in_range_from_start(
        client: &Client,
        stream: &str,
        current_rev: u64,
        event_id: Uuid,
    ) -> Result<Option<WriteResult>> {
        let opts = ReadStreamOptions::default()
            .forwards()
            .position(StreamPosition::Start)
            .max_count((current_rev as usize) + 1);
        run_reconcile_scan(client, stream, opts, event_id).await
    }

    async fn run_reconcile_scan(
        client: &Client,
        stream: &str,
        opts: ReadStreamOptions,
        event_id: Uuid,
    ) -> Result<Option<WriteResult>> {
        let mut read = match client.read_stream(stream, &opts).await {
            Ok(s) => s,
            Err(kurrentdb::Error::ResourceNotFound) => return Ok(None),
            Err(e) => return Err(anyhow!("kurrent reconcile read failed: {e}")),
        };
        while let Some(resolved) = read
            .next()
            .await
            .map_err(|e| anyhow!("kurrent reconcile next failed: {e}"))?
        {
            let rec = resolved.get_original_event();
            if rec.id == event_id {
                return Ok(Some(WriteResult {
                    position: LogCursor::from_raw(rec.position.commit),
                    revision: Some(StreamRevision::from_raw(rec.revision)),
                }));
            }
        }
        Ok(None)
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
            aggregate_type: Option<&str>,
            aggregate_id: Option<Uuid>,
        ) -> EventData {
            EventData {
                event_id:        Uuid::new_v4(),
                causation_id:       None,
                correlation_id:  Uuid::new_v4(),
                event_type:      event_type.to_string(),
                payload:         serde_json::json!({}),
                created_at:      Utc::now(),
                aggregate_type:  aggregate_type.map(String::from),
                aggregate_id,
                metadata:        Map::new(),
                ephemeral:       None,
                persistent:      true,
            }
        }

        #[test]
        fn stream_name_uses_aggregate_when_present() {
            let id = Uuid::new_v4();
            let e = mk_event("lifecycle:run_started", Some("lifecycle"), Some(id));
            assert_eq!(stream_name(&e), format!("lifecycle-{}", id));
        }

        #[test]
        fn stream_name_falls_back_to_global_for_non_aggregate() {
            let e = mk_event("telemetry:ping", None, None);
            assert_eq!(stream_name(&e), "telemetry-_global");
        }

        #[test]
        fn stream_name_uses_causal_when_event_type_has_no_colon() {
            let e = mk_event("order_placed", None, None);
            assert_eq!(stream_name(&e), "order_placed-_global");
            let e = mk_event("", None, None);
            assert_eq!(stream_name(&e), "causal-_global");
        }

        // The old "causal version vs Kurrent revision" conversion
        // helpers are gone in this release — causal::StreamRevision
        // is now 0-indexed and maps 1:1 to Kurrent's revision (no
        // off-by-one to test). End-to-end version semantics are
        // covered by the conformance suite running against a live
        // Kurrent (see
        // `tests/kurrent_event_log_conformance_test.rs`).

        #[test]
        fn build_metadata_stamps_reserved_keys() {
            let parent = Uuid::new_v4();
            let correlation = Uuid::new_v4();
            let mut e = mk_event("lifecycle:run", Some("lifecycle"), Some(Uuid::new_v4()));
            e.causation_id = Some(parent);
            e.correlation_id = correlation;
            e.metadata.insert("_run_id".into(), Value::String("r-1".into()));

            let m = build_metadata(&e);
            assert_eq!(
                m.get("$correlationId").and_then(Value::as_str),
                Some(correlation.to_string().as_str()),
                "Kurrent convention: $correlationId, not _correlation_id"
            );
            assert_eq!(
                m.get("$causationId").and_then(Value::as_str),
                Some(parent.to_string().as_str()),
                "Kurrent convention: $causationId, not _parent_id"
            );
            assert_eq!(m.get("_persistent").and_then(Value::as_bool), Some(true));
            assert_eq!(
                m.get("_aggregateType").and_then(Value::as_str),
                Some("lifecycle")
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
