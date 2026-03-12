//! Describe DSL for the causal inspector UI.
//!
//! Provides [`Block`] — a set of visual primitives that handlers return to
//! describe their current state. Blocks are serialized to JSON in the store
//! and rendered inline on reactor nodes in the causal flow diagram.
//!
//! # Usage
//!
//! ```
//! use causal_inspector_utils::{Block, ChecklistItem, State};
//!
//! let blocks = vec![
//!     Block::progress("Scrape phases", 0.75),
//!     Block::checklist("Steps", vec![
//!         ChecklistItem::new("Fetch data", true),
//!         ChecklistItem::new("Transform", false),
//!     ]),
//!     Block::status("Pipeline", State::Running),
//! ];
//! ```

use serde::{Deserialize, Serialize};

/// A visual primitive rendered inline on reactor nodes in the causal flow diagram.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Simple text label.
    Label { text: String },

    /// Numeric counter showing progress toward a total.
    Counter { label: String, value: u32, total: u32 },

    /// Fractional progress bar (0.0 – 1.0).
    Progress { label: String, fraction: f32 },

    /// Checklist of items with done/not-done state.
    Checklist { label: String, items: Vec<ChecklistItem> },

    /// Key-value pair.
    KeyValue { key: String, value: String },

    /// Status indicator with a named state.
    Status { label: String, state: State },
}

/// A checklist item with text and done state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChecklistItem {
    pub text: String,
    pub done: bool,
}

/// Named state for a [`Block::Status`] indicator.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Waiting,
    Running,
    Done,
    Error,
}

// ── Convenience constructors ──

impl Block {
    pub fn label(text: impl Into<String>) -> Self {
        Block::Label { text: text.into() }
    }

    pub fn counter(label: impl Into<String>, value: u32, total: u32) -> Self {
        Block::Counter { label: label.into(), value, total }
    }

    pub fn progress(label: impl Into<String>, fraction: f32) -> Self {
        Block::Progress { label: label.into(), fraction: fraction.clamp(0.0, 1.0) }
    }

    pub fn checklist(label: impl Into<String>, items: Vec<ChecklistItem>) -> Self {
        Block::Checklist { label: label.into(), items }
    }

    pub fn key_value(key: impl Into<String>, value: impl Into<String>) -> Self {
        Block::KeyValue { key: key.into(), value: value.into() }
    }

    pub fn status(label: impl Into<String>, state: State) -> Self {
        Block::Status { label: label.into(), state }
    }
}

impl ChecklistItem {
    pub fn new(text: impl Into<String>, done: bool) -> Self {
        Self { text: text.into(), done }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_serializes_with_type_tag() {
        let block = Block::progress("Loading", 0.5);
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "progress");
        assert_eq!(json["label"], "Loading");
        assert_eq!(json["fraction"], 0.5);
    }

    #[test]
    fn checklist_roundtrips() {
        let block = Block::checklist("Steps", vec![
            ChecklistItem::new("A", true),
            ChecklistItem::new("B", false),
        ]);
        let json = serde_json::to_string(&block).unwrap();
        let back: Block = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn state_serializes_snake_case() {
        let json = serde_json::to_value(State::Running).unwrap();
        assert_eq!(json, "running");
    }
}
