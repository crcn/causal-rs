//! Mermaid diagram rendering with component grouping (P2)

use crate::types::{CausalChain, EventSpan, SpanGraph};
use std::collections::{HashMap, HashSet};

/// Options for rendering diagrams
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Include state diffs in nodes
    pub show_state_diffs: bool,

    /// Include timing information
    pub show_timings: bool,

    /// Group by component (module_path)
    pub group_by_component: bool,

    /// Maximum depth to render (None = unlimited)
    pub max_depth: Option<usize>,

    /// Show only failed spans
    pub failed_only: bool,

    /// Direction: "TD" (top-down), "LR" (left-right)
    pub direction: String,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            show_state_diffs: false,
            show_timings: true,
            group_by_component: true,
            max_depth: None,
            failed_only: false,
            direction: "TD".into(),
        }
    }
}

/// Mermaid diagram renderer
pub struct MermaidRenderer {
    options: RenderOptions,
}

impl MermaidRenderer {
    pub fn new(options: RenderOptions) -> Self {
        Self { options }
    }

    /// Render a span graph as a Mermaid diagram
    pub fn render(&self, graph: &SpanGraph) -> String {
        let chains = graph.causal_chains();

        if chains.is_empty() {
            return "graph TD\n    Empty[No events captured]".to_string();
        }

        let mut output = format!("graph {}\n", self.options.direction);

        if self.options.group_by_component {
            output.push_str(&self.render_with_components(&chains));
        } else {
            output.push_str(&self.render_flat(&chains));
        }

        output
    }

    /// Render with component subgraphs (P2 priority)
    fn render_with_components(&self, chains: &[CausalChain]) -> String {
        let mut output = String::new();
        let mut components: HashMap<String, Vec<&EventSpan>> = HashMap::new();
        let mut edges = Vec::new();

        // Group spans by component
        for chain in chains {
            chain.walk(|span| {
                if self.should_render(span) {
                    let component = span.component();
                    components.entry(component).or_default().push(span);
                }
            });
        }

        // Render subgraphs for each component
        for (component, spans) in &components {
            output.push_str(&format!("    subgraph {}\n", sanitize_id(component)));

            for span in spans {
                output.push_str(&format!("        {}\n", self.render_node(span)));
            }

            output.push_str("    end\n");
        }

        // Collect edges across all chains
        for chain in chains {
            self.collect_edges(chain, &mut edges);
        }

        // Render edges
        for (from, to, label) in edges {
            output.push_str(&format!("    {} -->|{}| {}\n", from, label, to));
        }

        output
    }

    /// Render flat (no component grouping)
    fn render_flat(&self, chains: &[CausalChain]) -> String {
        let mut output = String::new();
        let mut rendered: HashSet<String> = HashSet::new();
        let mut edges = Vec::new();

        // Render all nodes
        for chain in chains {
            chain.walk(|span| {
                if self.should_render(span) {
                    let node_id = self.node_id(span);
                    if !rendered.contains(&node_id) {
                        output.push_str(&format!("    {}\n", self.render_node(span)));
                        rendered.insert(node_id);
                    }
                }
            });
        }

        // Collect and render edges
        for chain in chains {
            self.collect_edges(chain, &mut edges);
        }

        for (from, to, label) in edges {
            output.push_str(&format!("    {} -->|{}| {}\n", from, label, to));
        }

        output
    }

    /// Render a single node
    fn render_node(&self, span: &EventSpan) -> String {
        let node_id = self.node_id(span);
        let label = self.node_label(span);

        // Node shape based on status
        let (open, close) = if !span.success {
            ("{", "}") // Hexagon for errors
        } else if span.effect_name.is_some() {
            ("[", "]") // Rectangle for effects
        } else {
            ("((", "))") // Circle for events
        };

        format!("{}{}\"{}\"{}", node_id, open, label, close)
    }

    /// Generate node label
    fn node_label(&self, span: &EventSpan) -> String {
        let mut parts = vec![span.event_type.clone()];

        if let Some(effect) = &span.effect_name {
            parts.push(format!("Effect: {}", effect));
        }

        if self.options.show_timings {
            if let Some(duration_us) = span.duration_us {
                if duration_us > 1000 {
                    parts.push(format!("{}ms", duration_us / 1000));
                } else {
                    parts.push(format!("{}μs", duration_us));
                }
            }
        }

        if self.options.show_state_diffs {
            if let Some(diff) = &span.state_diff {
                if !diff.is_null() {
                    parts.push(format!("State: {}", truncate(diff.to_string(), 30)));
                }
            }
        }

        if let Some(err) = &span.error {
            parts.push(format!("Error: {}", truncate(err.clone(), 40)));
        }

        parts.join("<br/>")
    }

    /// Generate unique node ID
    fn node_id(&self, span: &EventSpan) -> String {
        format!("N{}", span.event_id.to_string().replace("-", "")[..8].to_string())
    }

    /// Collect edges recursively
    fn collect_edges(&self, chain: &CausalChain, edges: &mut Vec<(String, String, String)>) {
        let parent_id = self.node_id(&chain.span);

        for child in &chain.children {
            if self.should_render(&child.span) {
                let child_id = self.node_id(&child.span);
                let label = child.span.effect_name.clone().unwrap_or_else(|| "".into());

                edges.push((parent_id.clone(), child_id, label));
                self.collect_edges(child, edges);
            }
        }
    }

    /// Check if span should be rendered
    fn should_render(&self, span: &EventSpan) -> bool {
        if self.options.failed_only && span.success {
            return false;
        }
        true
    }
}

/// Sanitize component name for Mermaid subgraph ID
fn sanitize_id(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// Truncate string with ellipsis
fn truncate(s: String, max_len: usize) -> String {
    if s.len() <= max_len {
        s
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EventSpan;
    use chrono::Utc;
    use std::any::TypeId;
    use uuid::Uuid;

    fn make_span(event_type: &str, module_path: Option<&str>) -> EventSpan {
        EventSpan {
            span_id: Uuid::new_v4(),
            event_id: Uuid::new_v4(),
            parent_event_id: None,
            event_type: event_type.into(),
            event_type_id: TypeId::of::<()>(),
            effect_name: Some("test_effect".into()),
            module_path: module_path.map(|s| s.into()),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            duration_us: Some(1000),
            prev_state: None,
            next_state: None,
            state_diff: None,
            success: true,
            error: None,
            attributes: HashMap::new(),
        }
    }

    #[test]
    fn test_render_empty_graph() {
        let graph = SpanGraph::default();
        let renderer = MermaidRenderer::new(RenderOptions::default());

        let diagram = renderer.render(&graph);
        assert!(diagram.contains("No events captured"));
    }

    #[test]
    fn test_render_with_components() {
        let mut graph = SpanGraph::default();
        graph.add_span(make_span("OrderPlaced", Some("orders::handlers")));
        graph.add_span(make_span("PaymentProcessed", Some("payments::handlers")));

        let renderer = MermaidRenderer::new(RenderOptions {
            group_by_component: true,
            ..Default::default()
        });

        let diagram = renderer.render(&graph);
        assert!(diagram.contains("subgraph"));
        assert!(diagram.contains("OrderPlaced"));
        assert!(diagram.contains("PaymentProcessed"));
    }

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("my::module::name"), "my__module__name");
        assert_eq!(sanitize_id("simple"), "simple");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short".into(), 10), "short");
        assert_eq!(truncate("this is a very long string".into(), 10), "this is...");
    }
}
