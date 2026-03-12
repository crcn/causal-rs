import { useMemo, useCallback, memo } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  Handle,
  MarkerType,
  type Node,
  type Edge,
  type NodeProps,
  Position,
} from "@xyflow/react";
import dagre from "@dagrejs/dagre";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent } from "../types";
import { eventBg, eventBorder, eventTextColor } from "../theme";

// ---------------------------------------------------------------------------
// Topology derivation
// ---------------------------------------------------------------------------

type TopologyGraph = {
  eventTypes: Map<string, { name: string; count: number; isRoot: boolean }>;
  reactors: Map<string, { id: string; inputTypes: Set<string>; outputTypes: Set<string>; count: number }>;
};

function deriveTopology(events: InspectorEvent[]): TopologyGraph {
  const eventTypes = new Map<string, { name: string; count: number; isRoot: boolean }>();
  const reactors = new Map<string, { id: string; inputTypes: Set<string>; outputTypes: Set<string>; count: number }>();

  // Build lookup: eventId → event
  const byId = new Map<string, InspectorEvent>();
  for (const e of events) {
    if (e.id) byId.set(e.id, e);
  }

  for (const event of events) {
    // Track event type
    const et = eventTypes.get(event.name) ?? { name: event.name, count: 0, isRoot: !event.reactorId };
    et.count++;
    if (!event.reactorId) et.isRoot = true;
    eventTypes.set(event.name, et);

    // If this event was produced by a reactor, build the edge
    if (event.reactorId) {
      const reactor = reactors.get(event.reactorId) ?? {
        id: event.reactorId,
        inputTypes: new Set<string>(),
        outputTypes: new Set<string>(),
        count: 0,
      };
      reactor.outputTypes.add(event.name);
      reactor.count++;

      // Find parent event to determine what triggered this reactor
      if (event.parentId) {
        const parent = byId.get(event.parentId);
        if (parent) {
          reactor.inputTypes.add(parent.name);
        }
      }

      reactors.set(event.reactorId, reactor);
    }
  }

  return { eventTypes, reactors };
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

const ET_NODE_WIDTH = 160;
const ET_NODE_HEIGHT = 40;
const REACTOR_NODE_WIDTH = 150;
const REACTOR_NODE_HEIGHT = 32;

function layoutTopology(topology: TopologyGraph): { nodes: Node[]; edges: Edge[] } {
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: "LR", nodesep: 40, ranksep: 80, marginx: 20, marginy: 20 });
  g.setDefaultEdgeLabel(() => ({}));

  const nodes: Node[] = [];
  const edges: Edge[] = [];
  const maxCount = Math.max(
    1,
    ...Array.from(topology.reactors.values()).map((r) => r.count),
  );

  // Add event type nodes
  for (const [name, data] of topology.eventTypes) {
    const id = `et:${name}`;
    g.setNode(id, { width: ET_NODE_WIDTH, height: ET_NODE_HEIGHT });
    nodes.push({
      id,
      type: "eventTypeNode",
      position: { x: 0, y: 0 },
      data: { name, count: data.count, isRoot: data.isRoot },
    });
  }

  // Add reactor nodes
  for (const [reactorId, data] of topology.reactors) {
    const id = `r:${reactorId}`;
    g.setNode(id, { width: REACTOR_NODE_WIDTH, height: REACTOR_NODE_HEIGHT });
    nodes.push({
      id,
      type: "reactorNode",
      position: { x: 0, y: 0 },
      data: { reactorId, count: data.count },
    });

    // Edges: inputType → reactor
    for (const inputType of data.inputTypes) {
      const edgeId = `et:${inputType}->r:${reactorId}`;
      const strokeWidth = 1 + (data.count / maxCount) * 3;
      edges.push({
        id: edgeId,
        source: `et:${inputType}`,
        target: id,
        type: "default",
        style: { strokeWidth, stroke: "#52525b" },
        markerEnd: { type: MarkerType.ArrowClosed, width: 12, height: 12, color: "#52525b" },
      });
    }

    // Edges: reactor → outputType
    for (const outputType of data.outputTypes) {
      const edgeId = `r:${reactorId}->et:${outputType}`;
      const strokeWidth = 1 + (data.count / maxCount) * 3;
      edges.push({
        id: edgeId,
        source: id,
        target: `et:${outputType}`,
        type: "default",
        style: { strokeWidth, stroke: "#52525b" },
        markerEnd: { type: MarkerType.ArrowClosed, width: 12, height: 12, color: "#52525b" },
      });
    }
  }

  // Run dagre layout
  dagre.layout(g);

  // Apply positions
  for (const node of nodes) {
    const pos = g.node(node.id);
    if (pos) {
      const w = node.type === "eventTypeNode" ? ET_NODE_WIDTH : REACTOR_NODE_WIDTH;
      const h = node.type === "eventTypeNode" ? ET_NODE_HEIGHT : REACTOR_NODE_HEIGHT;
      node.position = { x: pos.x - w / 2, y: pos.y - h / 2 };
    }
  }

  return { nodes, edges };
}

// ---------------------------------------------------------------------------
// Custom nodes
// ---------------------------------------------------------------------------

const EventTypeNode = memo(({ data }: NodeProps) => {
  const d = data as { name: string; count: number; isRoot: boolean };
  return (
    <div
      style={{
        width: ET_NODE_WIDTH,
        height: ET_NODE_HEIGHT,
        background: eventBg(d.name),
        border: `1px solid ${eventBorder(d.name)}`,
        borderRadius: 8,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        padding: "0 8px",
        cursor: "pointer",
      }}
    >
      <Handle type="target" position={Position.Left} style={{ background: "#52525b", width: 6, height: 6 }} />
      <div style={{ textAlign: "center" }}>
        <div style={{ fontSize: 11, fontWeight: 600, color: eventTextColor(d.name), lineHeight: 1.2, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", maxWidth: ET_NODE_WIDTH - 20 }}>
          {d.name}
        </div>
        <div style={{ fontSize: 9, color: "#71717a" }}>
          {d.count}x{d.isRoot ? " (root)" : ""}
        </div>
      </div>
      <Handle type="source" position={Position.Right} style={{ background: "#52525b", width: 6, height: 6 }} />
    </div>
  );
});

const TopologyReactorNode = memo(({ data }: NodeProps) => {
  const d = data as { reactorId: string; count: number };
  return (
    <div
      style={{
        width: REACTOR_NODE_WIDTH,
        height: REACTOR_NODE_HEIGHT,
        background: "#27272a",
        border: "1px solid #3f3f46",
        borderRadius: 16,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        padding: "0 8px",
        cursor: "pointer",
      }}
    >
      <Handle type="target" position={Position.Left} style={{ background: "#52525b", width: 6, height: 6 }} />
      <div style={{ textAlign: "center" }}>
        <div style={{ fontSize: 10, fontWeight: 500, color: "#e4e4e7", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", maxWidth: REACTOR_NODE_WIDTH - 20 }}>
          {d.reactorId}
        </div>
        <div style={{ fontSize: 8, color: "#71717a" }}>{d.count}x</div>
      </div>
      <Handle type="source" position={Position.Right} style={{ background: "#52525b", width: 6, height: 6 }} />
    </div>
  );
});

const nodeTypes = {
  eventTypeNode: EventTypeNode,
  reactorNode: TopologyReactorNode,
};

// ---------------------------------------------------------------------------
// TopologyPane
// ---------------------------------------------------------------------------

export type TopologyPaneProps = Record<string, never>;

export function TopologyPane() {
  const events = useSelector<InspectorState, InspectorEvent[]>((s) => s.events);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const topology = useMemo(() => deriveTopology(events), [events]);
  const { nodes, edges } = useMemo(() => layoutTopology(topology), [topology]);

  const onNodeClick = useCallback(
    (_: React.MouseEvent, node: Node) => {
      if (node.type === "eventTypeNode") {
        const d = node.data as { name: string };
        dispatch({ type: "ui/filter_changed", payload: { search: d.name } });
      } else if (node.type === "reactorNode") {
        const d = node.data as { reactorId: string };
        dispatch({ type: "ui/filter_changed", payload: { search: d.reactorId } });
      }
    },
    [dispatch],
  );

  if (events.length === 0) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#71717a", fontSize: 13 }}>
        No events yet — submit data to see the topology
      </div>
    );
  }

  return (
    <div style={{ height: "100%", width: "100%" }}>
      <ReactFlow
        nodes={nodes}
        edges={edges}
        nodeTypes={nodeTypes}
        onNodeClick={onNodeClick}
        fitView
        minZoom={0.3}
        maxZoom={2}
        proOptions={{ hideAttribution: true }}
      >
        <Background color="#27272a" gap={16} />
        <Controls showInteractive={false} />
      </ReactFlow>
    </div>
  );
}
