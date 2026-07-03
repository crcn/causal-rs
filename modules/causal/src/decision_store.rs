//! `DecisionStore` — one durable decision per `(consumer, trigger)`.
//!
//! ## Why this exists
//!
//! Reactors are at-least-once catch-up consumers, so the same trigger can
//! be **redelivered** (crash between output append and cursor advance,
//! deploy overlap, lease handoff). Historically redelivery *re-decided*:
//! the reactor body re-ran and the runtime hoped recomputation was
//! byte-identical. For a nondeterministic body that hope is unenforceable,
//! and the failure is silent — two executions can emit *disjoint* output
//! sets that merge into a "chimera" log: a decision no single execution
//! ever made.
//!
//! [`EffectStore`](crate::effect_store) memoizes an *individual external
//! call* so re-running the body is cheaper; it does not stop the body from
//! re-deciding. `DecisionStore` closes the gap one level up: it durably
//! records the **whole output batch** a reaction produced, keyed by
//! `(consumer, trigger_event_id)`. The rules that make it work:
//!
//! 1. **A trigger's outputs enter the log only from a sealed record.** Even
//!    the sealing execution appends from the canonical row the store
//!    returns, not from its own local `outputs` — so two racing executions
//!    append the *same* batch regardless of which one sealed.
//! 2. **Seal is atomic and first-write-wins.** Exactly one decision per
//!    `(consumer, trigger)` ever exists.
//!
//! Redelivery then never runs the body: it replays the record, appending
//! any outputs a crash left un-appended (idempotent completion). A crash
//! *before* seal leaves no record, so re-deciding is correct — no decision
//! was ever made.
//!
//! `causal` owns the trait and the reference [`InMemoryDecisionStore`];
//! backends (`causal_replay`'s Postgres impl) supply durable storage.
//!
//! ## Durable projection & sanitization
//!
//! [`EventData`] carries an `ephemeral` typed handle for zero-cost
//! in-process dispatch that cannot be serialized. A record persists only
//! the *durable envelope* of each output (the fields a store-loaded event
//! carries); [`DecisionRecord::outputs_to_json`] /
//! [`DecisionRecord::outputs_from_json`] are the single canonical
//! conversion both backends share, so every backend round-trips
//! identically. That conversion also strips JSONB-hostile `\0`
//! sequences (scraped web content contains them) — an unsanitized seal
//! would fail *deterministically*, which under infra-retry is a permanent
//! wedge.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use crate::types::{EventData, LogCursor};

/// One durable decision: the full output batch a reaction produced for a
/// single trigger. See the module docs for the invariants that make this
/// the *only* source of a trigger's outputs.
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    /// The reacting consumer — `Reactor::NAME`.
    pub consumer: String,
    /// The triggering event's id.
    pub trigger_event_id: Uuid,
    /// The triggering event's log position. Retention GC compares this to
    /// the consumer's durable ack-floor so a record is never removed while
    /// its trigger is still redeliverable (A1's floor-minimum bound).
    pub trigger_position: LogCursor,
    /// The full output envelopes, in emit order. May be empty — a
    /// zero-output reaction seals an empty batch, distinguishing
    /// "processed, decided nothing" from "never ran". For a PARKED decision
    /// (`parked = true`) this is the terminal-failure fact (or empty, for a
    /// silently-parked reaction: cycle-guard / mapper-`None`).
    pub outputs: Vec<EventData>,
    /// Whether this decision is a terminal PARK (the reaction failed and was
    /// sent to the DLQ) rather than a success. A parked record replays its
    /// terminal fact on redelivery and keeps the trigger's effect entries at
    /// floor-GC (failure replay restores from them). Default `false`.
    pub parked: bool,
    /// When the decision was sealed.
    pub sealed_at: DateTime<Utc>,
}

/// The durable, serializable subset of an [`EventData`]. Mirrors what a
/// store-loaded event carries; the non-serializable `ephemeral` handle and
/// the always-true `persistent` flag are intentionally dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutputEnvelope {
    event_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    causation_id: Option<Uuid>,
    workflow_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    metadata: serde_json::Map<String, serde_json::Value>,
}

impl OutputEnvelope {
    fn from_event(e: &EventData) -> Self {
        Self {
            event_id: e.event_id,
            causation_id: e.causation_id,
            workflow_id: e.workflow_id,
            event_type: e.event_type.clone(),
            payload: e.payload.clone(),
            created_at: e.created_at,
            category: e.category.clone(),
            subject_id: e.subject_id,
            metadata: e.metadata.clone(),
        }
    }

    fn into_event(self) -> EventData {
        EventData {
            event_id: self.event_id,
            causation_id: self.causation_id,
            workflow_id: self.workflow_id,
            event_type: self.event_type,
            payload: self.payload,
            created_at: self.created_at,
            category: self.category,
            subject_id: self.subject_id,
            metadata: self.metadata,
            // A record is durable storage: no in-process typed handle, and
            // facts are always persistent (matches store-loaded events).
            ephemeral: None,
            persistent: true,
        }
    }
}

/// Recursively strip `\0` from every string in a JSON value. Postgres
/// `jsonb` rejects the NUL codepoint; without this a seal of scraped
/// content fails deterministically. Applied at serialization time so every
/// backend (and the in-memory reference) sees identically-sanitized bytes.
fn sanitize_nul(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if s.contains('\u{0}') {
                *s = s.replace('\u{0}', "");
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sanitize_nul),
        serde_json::Value::Object(map) => map.values_mut().for_each(sanitize_nul),
        _ => {}
    }
}

impl DecisionRecord {
    /// Build a record stamped at `sealed_at`. Callers derive output
    /// event_ids before constructing the record (seal-time derivation).
    pub fn new(
        consumer: impl Into<String>,
        trigger_event_id: Uuid,
        trigger_position: LogCursor,
        outputs: Vec<EventData>,
        sealed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            consumer: consumer.into(),
            trigger_event_id,
            trigger_position,
            outputs,
            parked: false,
            sealed_at,
        }
    }

    /// Mark this record as a terminal PARK (builder-style). See
    /// [`parked`](Self::parked).
    pub fn with_parked(mut self, parked: bool) -> Self {
        self.parked = parked;
        self
    }

    /// Serialize `outputs` to the canonical durable JSON array, with
    /// `\0` stripped. This is the single conversion every backend uses
    /// to persist a record's batch.
    pub fn outputs_to_json(&self) -> Result<serde_json::Value> {
        let envelopes: Vec<OutputEnvelope> =
            self.outputs.iter().map(OutputEnvelope::from_event).collect();
        let mut value = serde_json::to_value(envelopes)?;
        sanitize_nul(&mut value);
        Ok(value)
    }

    /// Inverse of [`outputs_to_json`](Self::outputs_to_json): reconstruct
    /// the output envelopes a backend loaded. Loaded events carry no
    /// `ephemeral` handle.
    pub fn outputs_from_json(value: serde_json::Value) -> Result<Vec<EventData>> {
        let envelopes: Vec<OutputEnvelope> = serde_json::from_value(value)?;
        Ok(envelopes.into_iter().map(OutputEnvelope::into_event).collect())
    }
}

/// Durable, first-write-wins record of a reactor's decision, keyed by
/// `(consumer, trigger_event_id)`. See module docs.
#[async_trait]
pub trait DecisionStore: Send + Sync {
    /// Insert-if-absent, then return the **canonical** row — ours if we
    /// won the race, the pre-existing one otherwise. Callers MUST append
    /// from the returned record, never from the `rec` they passed in, so
    /// racing executions append identical batches.
    async fn seal(&self, rec: DecisionRecord) -> Result<DecisionRecord>;

    /// The sealed decision for `(consumer, trigger_event_id)`, if one
    /// exists.
    async fn get(
        &self,
        consumer: &str,
        trigger_event_id: Uuid,
    ) -> Result<Option<DecisionRecord>>;

    /// Delete the record for `(consumer, trigger_event_id)` (idempotent —
    /// absent is fine). Retention-based GC is driven by the engine; the
    /// store only exposes the primitive.
    async fn remove(&self, consumer: &str, trigger_event_id: Uuid) -> Result<()>;

    /// Retention GC honoring A1's **age AND floor-minimum** rule: remove
    /// `consumer`'s records that are BOTH sealed strictly before
    /// `aged_before` AND whose `trigger_position` is at or below `floor`
    /// (the consumer's durable ack-floor has passed the trigger, so no
    /// redelivery is expected). Returns rows removed.
    ///
    /// Both conditions are load-bearing:
    /// - **Age** bounds the zombie-reseal window — a stale lease-holder
    ///   (advisory locks carry no fencing token) can only re-seal within the
    ///   retention window, not indefinitely.
    /// - **Floor** ensures a still-redeliverable record is never dropped: a
    ///   record GC'd before its ack-floor passes would let a redelivery
    ///   re-decide (get-miss → body re-runs), reopening the chimera. Age
    ///   alone is insufficient — `sealed_at` is unrelated to whether the
    ///   trigger was acked (a lost ack, wedged partition, or short window
    ///   ages a record while it is still redeliverable).
    async fn remove_reclaimable(
        &self,
        consumer: &str,
        aged_before: DateTime<Utc>,
        floor: LogCursor,
    ) -> Result<u64>;

    /// Distinct consumer ids with sealed decisions. Powers boot-time orphan
    /// detection (D4). Best-effort — default empty.
    async fn list_consumers(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// In-memory [`DecisionStore`] for tests, examples, and single-process
/// use. No durability across restarts.
///
/// It round-trips every sealed record through
/// [`DecisionRecord::outputs_to_json`] / `outputs_from_json` so it behaves
/// *exactly* like a durable backend (sanitized payloads, dropped
/// `ephemeral`) — a faithful reference, not a lenient one.
#[derive(Default)]
pub struct InMemoryDecisionStore {
    inner: Mutex<HashMap<(String, Uuid), StoredDecision>>,
}

struct StoredDecision {
    outputs_json: serde_json::Value,
    sealed_at: DateTime<Utc>,
    trigger_position: LogCursor,
    parked: bool,
}

impl InMemoryDecisionStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn load(&self, consumer: &str, trigger: Uuid, s: &StoredDecision) -> Result<DecisionRecord> {
        Ok(DecisionRecord {
            consumer: consumer.to_string(),
            trigger_event_id: trigger,
            trigger_position: s.trigger_position,
            outputs: DecisionRecord::outputs_from_json(s.outputs_json.clone())?,
            parked: s.parked,
            sealed_at: s.sealed_at,
        })
    }
}

#[async_trait]
impl DecisionStore for InMemoryDecisionStore {
    async fn seal(&self, rec: DecisionRecord) -> Result<DecisionRecord> {
        let outputs_json = rec.outputs_to_json()?;
        let key = (rec.consumer.clone(), rec.trigger_event_id);
        let mut map = self.inner.lock().unwrap();
        let canonical = map.entry(key).or_insert(StoredDecision {
            outputs_json,
            sealed_at: rec.sealed_at,
            trigger_position: rec.trigger_position,
            parked: rec.parked,
        });
        self.load(&rec.consumer, rec.trigger_event_id, canonical)
    }

    async fn get(
        &self,
        consumer: &str,
        trigger_event_id: Uuid,
    ) -> Result<Option<DecisionRecord>> {
        let map = self.inner.lock().unwrap();
        match map.get(&(consumer.to_string(), trigger_event_id)) {
            Some(s) => Ok(Some(self.load(consumer, trigger_event_id, s)?)),
            None => Ok(None),
        }
    }

    async fn remove(&self, consumer: &str, trigger_event_id: Uuid) -> Result<()> {
        self.inner.lock().unwrap().remove(&(consumer.to_string(), trigger_event_id));
        Ok(())
    }

    async fn remove_reclaimable(
        &self,
        consumer: &str,
        aged_before: DateTime<Utc>,
        floor: LogCursor,
    ) -> Result<u64> {
        let mut map = self.inner.lock().unwrap();
        let before = map.len();
        map.retain(|(c, _), s| {
            // Keep unless: this consumer AND aged out AND floor has passed AND
            // NOT parked. A parked record is a terminal marker — reclaiming it
            // lets a later checkpoint regression re-deliver the trigger, whose
            // body may now SUCCEED and append outputs (disjoint event_ids from
            // the terminal fact, so no divergence reconciles them) alongside
            // the terminal fact: a park chimera. Parks are rare/exceptional, so
            // retaining them indefinitely is cheap and keeps the outcome final.
            !(c == consumer
                && s.sealed_at < aged_before
                && s.trigger_position <= floor
                && !s.parked)
        });
        Ok((before - map.len()) as u64)
    }

    async fn list_consumers(&self) -> Result<Vec<String>> {
        let map = self.inner.lock().unwrap();
        let set: std::collections::HashSet<&str> =
            map.keys().map(|(c, _)| c.as_str()).collect();
        Ok(set.into_iter().map(String::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(event_type: &str, subject: Uuid, payload: serde_json::Value) -> EventData {
        EventData {
            event_id: Uuid::new_v4(),
            causation_id: Some(Uuid::new_v4()),
            workflow_id: Uuid::new_v4(),
            event_type: event_type.to_string(),
            payload,
            created_at: Utc::now(),
            category: Some(event_type.to_string()),
            subject_id: Some(subject),
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        }
    }

    #[tokio::test]
    async fn seal_then_get_round_trips_outputs() {
        let store = InMemoryDecisionStore::new();
        let trigger = Uuid::new_v4();
        let outputs = vec![
            out("Out", Uuid::new_v4(), serde_json::json!({"n": 1})),
            out("Out", Uuid::new_v4(), serde_json::json!({"n": 2})),
        ];
        let rec = DecisionRecord::new("c", trigger, LogCursor::ZERO, outputs.clone(), Utc::now());
        let sealed = store.seal(rec).await.unwrap();
        assert_eq!(sealed.outputs.len(), 2);

        let got = store.get("c", trigger).await.unwrap().unwrap();
        assert_eq!(got.outputs.len(), 2);
        assert_eq!(got.outputs[0].event_id, outputs[0].event_id);
        assert_eq!(got.outputs[1].payload, serde_json::json!({"n": 2}));
    }

    #[tokio::test]
    async fn seal_is_first_write_wins() {
        let store = InMemoryDecisionStore::new();
        let trigger = Uuid::new_v4();

        let first = DecisionRecord::new(
            "c",
            trigger,
            LogCursor::ZERO,
            vec![out("Out", Uuid::new_v4(), serde_json::json!("first"))],
            Utc::now(),
        );
        let first_id = first.outputs[0].event_id;
        store.seal(first).await.unwrap();

        // A racing second seal (different outputs) must adopt the first.
        let second = DecisionRecord::new(
            "c",
            trigger,
            LogCursor::ZERO,
            vec![out("Out", Uuid::new_v4(), serde_json::json!("second"))],
            Utc::now(),
        );
        let canonical = store.seal(second).await.unwrap();
        assert_eq!(canonical.outputs.len(), 1);
        assert_eq!(canonical.outputs[0].event_id, first_id, "first write wins");
        assert_eq!(canonical.outputs[0].payload, serde_json::json!("first"));
    }

    #[tokio::test]
    async fn empty_record_round_trips() {
        let store = InMemoryDecisionStore::new();
        let trigger = Uuid::new_v4();
        store
            .seal(DecisionRecord::new("c", trigger, LogCursor::ZERO, vec![], Utc::now()))
            .await
            .unwrap();
        let got = store.get("c", trigger).await.unwrap();
        let got = got.expect("empty record is present, not absent");
        assert!(got.outputs.is_empty(), "sealed empty batch stays empty");
    }

    #[tokio::test]
    async fn records_isolated_by_consumer_and_trigger() {
        let store = InMemoryDecisionStore::new();
        let trigger = Uuid::new_v4();
        store
            .seal(DecisionRecord::new("a", trigger, LogCursor::ZERO, vec![], Utc::now()))
            .await
            .unwrap();
        assert!(store.get("b", trigger).await.unwrap().is_none(), "consumer isolation");
        assert!(store.get("a", Uuid::new_v4()).await.unwrap().is_none(), "trigger isolation");
    }

    #[tokio::test]
    async fn remove_makes_record_absent_and_is_idempotent() {
        let store = InMemoryDecisionStore::new();
        let trigger = Uuid::new_v4();
        store
            .seal(DecisionRecord::new("c", trigger, LogCursor::ZERO, vec![], Utc::now()))
            .await
            .unwrap();
        store.remove("c", trigger).await.unwrap();
        assert!(store.get("c", trigger).await.unwrap().is_none());
        store.remove("c", trigger).await.unwrap(); // idempotent
    }

    #[tokio::test]
    async fn retention_gc_never_reclaims_a_parked_record() {
        // A parked record is a terminal marker: retention GC must keep it even
        // when it is aged AND the floor has passed, or a later checkpoint
        // regression re-decides the trigger into a park chimera. A success
        // record in the identical age/floor position IS reclaimed.
        let store = InMemoryDecisionStore::new();
        let aged = Utc::now() - chrono::Duration::days(30);
        let park = Uuid::new_v4();
        let win = Uuid::new_v4();
        store
            .seal(
                DecisionRecord::new("c", park, LogCursor::from_raw(1), vec![], aged)
                    .with_parked(true),
            )
            .await
            .unwrap();
        store
            .seal(DecisionRecord::new("c", win, LogCursor::from_raw(1), vec![], aged))
            .await
            .unwrap();

        // Window fully elapsed and floor well past both triggers.
        let removed = store
            .remove_reclaimable("c", Utc::now(), LogCursor::from_raw(100))
            .await
            .unwrap();
        assert_eq!(removed, 1, "only the success record is reclaimed");
        assert!(
            store.get("c", park).await.unwrap().is_some_and(|r| r.parked),
            "parked record survives retention GC (terminal marker)",
        );
        assert!(
            store.get("c", win).await.unwrap().is_none(),
            "success record aged past the floor is reclaimed",
        );
    }

    #[tokio::test]
    async fn nul_bytes_stripped_from_payload_at_seal() {
        let store = InMemoryDecisionStore::new();
        let trigger = Uuid::new_v4();
        let dirty = out("Out", Uuid::new_v4(), serde_json::json!({"text": "a\u{0}b"}));
        store
            .seal(DecisionRecord::new("c", trigger, LogCursor::ZERO, vec![dirty], Utc::now()))
            .await
            .unwrap();
        let got = store.get("c", trigger).await.unwrap().unwrap();
        assert_eq!(got.outputs[0].payload["text"], serde_json::json!("ab"));
    }
}
