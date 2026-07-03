//! zz_audit_v16 — verifier for audit finding #16.
//!
//! Claim: `sanitize_nul` (decision_store.rs) strips NUL only from JSON
//! string VALUES, not object KEYS. The module docs promise the canonical
//! durable conversion (`DecisionRecord::outputs_to_json`) strips
//! JSONB-hostile `\u{0}` so a seal can never fail deterministically
//! (A4a). Postgres jsonb rejects ` ` ANYWHERE, keys included, so a
//! NUL surviving in a key means PgDecisionStore::seal fails on every
//! attempt and the reactor's infra-retry arm loops forever.
//!
//! CORRECT behavior: no NUL codepoint anywhere in the serialized durable
//! JSON. The defect makes the `key` assertions FAIL.

use causal::types::EventData;
use causal::{DecisionRecord, DecisionStore, InMemoryDecisionStore};
use chrono::Utc;
use uuid::Uuid;

fn output(payload: serde_json::Value) -> EventData {
    EventData {
        event_id: Uuid::new_v4(),
        causation_id: Some(Uuid::new_v4()),
        workflow_id: Uuid::new_v4(),
        event_type: "Out".to_string(),
        payload,
        created_at: Utc::now(),
        category: Some("Out".to_string()),
        subject_id: Some(Uuid::new_v4()),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

/// True if any string — value OR object key — contains U+0000.
fn contains_nul(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => s.contains('\u{0}'),
        serde_json::Value::Array(a) => a.iter().any(contains_nul),
        serde_json::Value::Object(m) => {
            m.iter().any(|(k, val)| k.contains('\u{0}') || contains_nul(val))
        }
        _ => false,
    }
}

#[tokio::test]
async fn control_nul_in_string_value_is_stripped() {
    // The case the sanitizer (and conformance DS9) covers today.
    let rec = DecisionRecord::new(
        "audit-v16",
        Uuid::new_v4(),
        vec![output(serde_json::json!({"text": "a\u{0}b"}))],
        Utc::now(),
    );
    let json = rec.outputs_to_json().expect("serialize");
    assert!(
        !contains_nul(&json),
        "control: NUL in a string VALUE must be stripped (it is today)"
    );
}

#[tokio::test]
async fn nul_in_object_key_must_also_be_stripped() {
    // The amendment's own threat model: payload map keyed by scraped
    // content. serde_json accepts NUL in keys; Postgres jsonb does not.
    let rec = DecisionRecord::new(
        "audit-v16",
        Uuid::new_v4(),
        vec![output(serde_json::json!({"a\u{0}b": 1}))],
        Utc::now(),
    );
    let json = rec.outputs_to_json().expect("serialize");
    let wire = serde_json::to_string(&json).unwrap();
    assert!(
        !contains_nul(&json),
        "DEFECT: NUL survives in an object KEY of the canonical durable \
         JSON (wire text contains {}). Postgres jsonb rejects \\u0000 in \
         keys, so PgDecisionStore::seal fails deterministically and the \
         reactor infra-retry arm loops forever. wire = {wire:?}",
        if wire.contains("\\u0000") { "\\u0000" } else { "?" },
    );
}

#[tokio::test]
async fn in_memory_store_cannot_catch_the_key_case() {
    // Demonstrates why `cargo test -p causal` never sees the failure:
    // the in-memory reference round-trips through the same canonical
    // conversion, but serde_json tolerates NUL in keys, so seal succeeds
    // where Postgres would error every attempt.
    let store = InMemoryDecisionStore::new();
    let rec = DecisionRecord::new(
        "audit-v16-mem",
        Uuid::new_v4(),
        vec![output(serde_json::json!({"a\u{0}b": 1}))],
        Utc::now(),
    );
    let sealed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.seal(rec),
    )
    .await
    .expect("no wedge in-memory")
    .expect("in-memory seal accepts NUL keys");
    // And the NUL key round-trips intact — in-memory is blind to it.
    assert!(
        contains_nul(&serde_json::to_value(&sealed.outputs[0].payload).unwrap()),
        "in-memory reference silently accepts + preserves the NUL key"
    );
}
