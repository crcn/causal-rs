//! State formatting and diffing for visualization

use serde::Serialize;
use serde_json::Value;
use std::fmt;

/// Trait for formatting state snapshots for visualization
///
/// Implement this to customize how state is displayed in diagrams and diffs.
pub trait StateFormatter<S>: Send + Sync {
    /// Serialize state to JSON for storage
    fn serialize(&self, state: &S) -> Result<Value, FormatterError>;

    /// Compute a diff between two states
    ///
    /// Returns a JSON value representing the changes, or None if states are identical.
    fn diff(&self, prev: &S, next: &S) -> Result<Option<Value>, FormatterError>;

    /// Format state for human-readable display
    fn display(&self, state: &S) -> Result<String, FormatterError> {
        Ok(self.serialize(state)?.to_string())
    }

    /// Check if state has changed (optimization for diff)
    fn has_changed(&self, prev: &S, next: &S) -> bool {
        self.diff(prev, next).ok().flatten().is_some()
    }
}

/// Default JSON-based formatter using serde
pub struct JsonDiffFormatter;

impl<S: Serialize + PartialEq> StateFormatter<S> for JsonDiffFormatter {
    fn serialize(&self, state: &S) -> Result<Value, FormatterError> {
        serde_json::to_value(state).map_err(|e| FormatterError::Serialization(e.to_string()))
    }

    fn diff(&self, prev: &S, next: &S) -> Result<Option<Value>, FormatterError> {
        // Fast path: check equality first
        if prev == next {
            return Ok(None);
        }

        let prev_val = self.serialize(prev)?;
        let next_val = self.serialize(next)?;

        // Compute JSON diff
        let diff = compute_json_diff(&prev_val, &next_val);

        if diff.is_null() {
            Ok(None)
        } else {
            Ok(Some(diff))
        }
    }
}

/// Compact formatter that omits unchanged fields
pub struct CompactFormatter;

impl<S: Serialize + PartialEq> StateFormatter<S> for CompactFormatter {
    fn serialize(&self, state: &S) -> Result<Value, FormatterError> {
        serde_json::to_value(state).map_err(|e| FormatterError::Serialization(e.to_string()))
    }

    fn diff(&self, prev: &S, next: &S) -> Result<Option<Value>, FormatterError> {
        if prev == next {
            return Ok(None);
        }

        let prev_val = self.serialize(prev)?;
        let next_val = self.serialize(next)?;

        // Only include changed fields
        let diff = compute_compact_diff(&prev_val, &next_val);

        if diff.is_null() {
            Ok(None)
        } else {
            Ok(Some(diff))
        }
    }

    fn display(&self, state: &S) -> Result<String, FormatterError> {
        // Only show non-null fields
        let val = self.serialize(state)?;
        let compact = compact_display(&val);
        Ok(serde_json::to_string_pretty(&compact)
            .map_err(|e| FormatterError::Serialization(e.to_string()))?)
    }
}

/// Compute a JSON diff between two values
///
/// Returns a JSON object showing changes:
/// - `{ "field": { "old": <prev>, "new": <next> } }` for changed fields
/// - `{ "field": { "added": <value> } }` for new fields
/// - `{ "field": { "removed": <value> } }` for deleted fields
fn compute_json_diff(prev: &Value, next: &Value) -> Value {
    match (prev, next) {
        (Value::Object(prev_map), Value::Object(next_map)) => {
            let mut diff = serde_json::Map::new();

            // Check for changed or removed fields
            for (key, prev_val) in prev_map {
                match next_map.get(key) {
                    Some(next_val) if prev_val != next_val => {
                        let mut change = serde_json::Map::new();
                        change.insert("old".to_string(), prev_val.clone());
                        change.insert("new".to_string(), next_val.clone());
                        diff.insert(key.clone(), Value::Object(change));
                    }
                    None => {
                        let mut change = serde_json::Map::new();
                        change.insert("removed".to_string(), prev_val.clone());
                        diff.insert(key.clone(), Value::Object(change));
                    }
                    _ => {} // Unchanged
                }
            }

            // Check for added fields
            for (key, next_val) in next_map {
                if !prev_map.contains_key(key) {
                    let mut change = serde_json::Map::new();
                    change.insert("added".to_string(), next_val.clone());
                    diff.insert(key.clone(), Value::Object(change));
                }
            }

            if diff.is_empty() {
                Value::Null
            } else {
                Value::Object(diff)
            }
        }
        _ if prev == next => Value::Null,
        _ => {
            let mut change = serde_json::Map::new();
            change.insert("old".to_string(), prev.clone());
            change.insert("new".to_string(), next.clone());
            Value::Object(change)
        }
    }
}

/// Compute a compact diff showing only changed values
fn compute_compact_diff(prev: &Value, next: &Value) -> Value {
    match (prev, next) {
        (Value::Object(prev_map), Value::Object(next_map)) => {
            let mut diff = serde_json::Map::new();

            for (key, next_val) in next_map {
                if let Some(prev_val) = prev_map.get(key) {
                    if prev_val != next_val {
                        diff.insert(key.clone(), next_val.clone());
                    }
                } else {
                    diff.insert(key.clone(), next_val.clone());
                }
            }

            if diff.is_empty() {
                Value::Null
            } else {
                Value::Object(diff)
            }
        }
        _ if prev == next => Value::Null,
        _ => next.clone(),
    }
}

/// Compact display: omit null fields
fn compact_display(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let compact: serde_json::Map<_, _> = map
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), compact_display(v)))
                .collect();
            Value::Object(compact)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(compact_display).collect()),
        _ => val.clone(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FormatterError {
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Diff computation failed: {0}")]
    DiffFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Debug, Clone, PartialEq, Serialize)]
    struct TestState {
        count: u32,
        name: String,
    }

    #[test]
    fn test_json_diff_formatter() {
        let formatter = JsonDiffFormatter;

        let prev = TestState {
            count: 1,
            name: "Alice".into(),
        };
        let next = TestState {
            count: 2,
            name: "Alice".into(),
        };

        let diff = formatter.diff(&prev, &next).unwrap();
        assert!(diff.is_some());

        let diff_val = diff.unwrap();
        assert!(diff_val.get("count").is_some());
        assert!(diff_val.get("name").is_none()); // Unchanged
    }

    #[test]
    fn test_no_diff_when_equal() {
        let formatter = JsonDiffFormatter;

        let state = TestState {
            count: 1,
            name: "Alice".into(),
        };

        let diff = formatter.diff(&state, &state).unwrap();
        assert!(diff.is_none());
    }

    #[test]
    fn test_compact_formatter() {
        let formatter = CompactFormatter;

        let prev = TestState {
            count: 1,
            name: "Alice".into(),
        };
        let next = TestState {
            count: 2,
            name: "Alice".into(),
        };

        let diff = formatter.diff(&prev, &next).unwrap().unwrap();

        // Compact should only show changed field
        assert!(diff.get("count").is_some());
        assert!(diff.get("name").is_none());
    }
}
