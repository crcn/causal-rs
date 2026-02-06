//! Workflow causality tree builder

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use uuid::Uuid;

/// Event node in the causality tree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventNode {
    pub event_id: Uuid,
    pub event_type: String,
    pub payload: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub hops: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<EventNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<EffectNode>,
}

/// Effect execution node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectNode {
    pub effect_id: String,
    pub status: String,
    pub attempts: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// Tree builder for workflow causality
pub struct TreeBuilder {
    pool: PgPool,
}

impl TreeBuilder {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Build causality tree for a workflow
    pub async fn build_tree(&self, correlation_id: Uuid) -> Result<Vec<EventNode>> {
        // Fetch all events for this workflow
        let events = self.fetch_events(correlation_id).await?;

        // Fetch all effects for this workflow
        let effects = self.fetch_effects(correlation_id).await?;

        // Build tree structure
        let tree = self.build_tree_structure(events, effects)?;

        Ok(tree)
    }

    async fn fetch_events(&self, correlation_id: Uuid) -> Result<Vec<EventRow>> {
        let rows = sqlx::query(
            r#"
            SELECT
                event_id,
                parent_id,
                event_type,
                payload,
                hops,
                batch_id,
                batch_index,
                batch_size,
                created_at
            FROM seesaw_events
            WHERE correlation_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        let events = rows
            .into_iter()
            .map(|row| EventRow {
                event_id: row.get("event_id"),
                parent_id: row.get("parent_id"),
                event_type: row.get("event_type"),
                payload: row.get("payload"),
                hops: row.get("hops"),
                batch_id: row.get("batch_id"),
                batch_index: row.get("batch_index"),
                batch_size: row.get("batch_size"),
                created_at: row.get("created_at"),
            })
            .collect();

        Ok(events)
    }

    async fn fetch_effects(&self, correlation_id: Uuid) -> Result<HashMap<Uuid, Vec<EffectNode>>> {
        let rows = sqlx::query(
            r#"
            SELECT
                event_id,
                effect_id,
                status,
                attempts,
                batch_id,
                batch_index,
                batch_size,
                error,
                completed_at
            FROM seesaw_effect_executions
            WHERE correlation_id = $1
            ORDER BY event_id, effect_id
            "#,
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        let mut effects_by_event: HashMap<Uuid, Vec<EffectNode>> = HashMap::new();

        for row in rows {
            let event_id: Uuid = row.get("event_id");
            let effect = EffectNode {
                effect_id: row.get("effect_id"),
                status: row.get("status"),
                attempts: row.get("attempts"),
                batch_id: row.get("batch_id"),
                batch_index: row.get("batch_index"),
                batch_size: row.get("batch_size"),
                error: row.get("error"),
                completed_at: row.get("completed_at"),
            };

            effects_by_event
                .entry(event_id)
                .or_insert_with(Vec::new)
                .push(effect);
        }

        Ok(effects_by_event)
    }

    fn build_tree_structure(
        &self,
        events: Vec<EventRow>,
        effects: HashMap<Uuid, Vec<EffectNode>>,
    ) -> Result<Vec<EventNode>> {
        // Build index of events by ID
        let mut events_by_id: HashMap<Uuid, EventRow> =
            events.into_iter().map(|e| (e.event_id, e)).collect();

        // Find root events (no parent)
        let roots: Vec<Uuid> = events_by_id
            .values()
            .filter(|e| e.parent_id.is_none())
            .map(|e| e.event_id)
            .collect();

        // Recursively build tree
        let tree = roots
            .into_iter()
            .filter_map(|root_id| self.build_node(root_id, &mut events_by_id, &effects))
            .collect();

        Ok(tree)
    }

    fn build_node(
        &self,
        event_id: Uuid,
        events: &mut HashMap<Uuid, EventRow>,
        effects: &HashMap<Uuid, Vec<EffectNode>>,
    ) -> Option<EventNode> {
        let event = events.remove(&event_id)?;

        // Find children (events with this event as parent)
        let child_ids: Vec<Uuid> = events
            .values()
            .filter(|e| e.parent_id == Some(event_id))
            .map(|e| e.event_id)
            .collect();

        let children = child_ids
            .into_iter()
            .filter_map(|child_id| self.build_node(child_id, events, effects))
            .collect();

        Some(EventNode {
            event_id: event.event_id,
            event_type: event.event_type,
            payload: event.payload,
            created_at: event.created_at,
            hops: event.hops,
            batch_id: event.batch_id,
            batch_index: event.batch_index,
            batch_size: event.batch_size,
            children,
            effects: effects.get(&event_id).cloned().unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone)]
struct EventRow {
    event_id: Uuid,
    parent_id: Option<Uuid>,
    event_type: String,
    payload: Option<serde_json::Value>,
    hops: i32,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_node_serialization() {
        let node = EventNode {
            event_id: Uuid::new_v4(),
            event_type: "OrderPlaced".to_string(),
            payload: Some(serde_json::json!({"order_id": 123})),
            created_at: Utc::now(),
            hops: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            children: vec![],
            effects: vec![EffectNode {
                effect_id: "validate_order".to_string(),
                status: "completed".to_string(),
                attempts: 1,
                batch_id: None,
                batch_index: None,
                batch_size: None,
                error: None,
                completed_at: Some(Utc::now()),
            }],
        };

        let json = serde_json::to_string(&node).expect("should serialize");
        let _deserialized: EventNode = serde_json::from_str(&json).expect("should deserialize");
    }

    #[test]
    fn event_node_deserializes_without_optional_arrays() {
        let json = serde_json::json!({
            "event_id": Uuid::new_v4(),
            "event_type": "OrderPlaced",
            "payload": { "order_id": 123 },
            "created_at": Utc::now(),
            "hops": 0
        })
        .to_string();

        let node: EventNode = serde_json::from_str(&json).expect("should deserialize");
        assert!(node.children.is_empty());
        assert!(node.effects.is_empty());
    }
}
