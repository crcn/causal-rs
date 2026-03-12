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
import { useSelector } from "../machine";
import type { InspectorState } from "../state";
import type { ReactorDependency } from "../types";
import { eventBg, eventBorder, eventTextColor } from "../theme";

// ---------------------------------------------------------------------------
// Node types
// ---------------------------------------------------------------------------

type DepNodeData =
  | { nodeKind: "event-type"; eventType: string }
  | { nodeKind: "reactor"; reactorId: string };

const EVENT_NODE_W = 160;
const EVENT_NODE_H = 36;
const REACTOR_NODE_W = 180;
const REACTOR_NODE_H = 40;

const EventTypeNode = memo(({ data }: NodeProps<Node<DepNodeData & { nodeKind: "event-type" }>>) => {
  const d = data as { nodeKind: "event-type"; eventType: string };
  return (
    <div
      style={{
        width: EVENT_NODE_W,
        height: EVENT_NODE_H,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        borderRadius: 6,
        border: `1px solid ${eventBorder(d.eventType)}`,
        background: eventBg(d.eventType),
        color: eventTextColor(d.eventType),
        fontSize: 11,
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
        fontWeight: 500,
        padding: "0 8px",
        overflow: "hidden",
        textOverflow: "ellipsis",
        whiteSpace: "nowrap",
      }}
      title={d.eventType}
    >
      <Handle type="target" position={Position.Left} style={{ opacity: 0 }} />
      {d.eventType}
      <Handle type="source" position={Position.Right} style={{ opacity: 0 }} />
    </div>
  );
});
EventTypeNode.displayName = "EventTypeNode";

const ReactorNode = memo(({ data }: NodeProps<Node<DepNodeData & { nodeKind: "reactor" }>>) => {
  const d = data as { nodeKind: "reactor"; reactorId: string };
  return (
    <div
      style={{
        width: REACTOR_NODE_W,
        height: REACTOR_NODE_H,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        borderRadius: 8,
        border: "1px solid #3f3f46",
        background: "#18181b",
        color: "#e4e4e7",
        fontSize: 11,
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
        fontWeight: 600,
        padding: "0 8px",
        overflow: "hidden",
        textOverflow: "ellipsis",
        whiteSpace: "nowrap",
      }}
      title={d.reactorId}
    >
      <Handle type="target" position={Position.Left} style={{ opacity: 0 }} />
      {d.reactorId}
      <Handle type="source" position={Position.Right} style={{ opacity: 0 }} />
    </div>
  );
});
ReactorNode.displayName = "ReactorNode";

const nodeTypes = {
  "event-type": EventTypeNode,
  reactor: ReactorNode,
};

// ---------------------------------------------------------------------------
// Graph builder
// ---------------------------------------------------------------------------

function buildDependencyGraph(deps: ReactorDependency[]): { nodes: Node<DepNodeData>[]; edges: Edge[] } {
  const nodes: Node<DepNodeData>[] = [];
  const edges: Edge[] = [];
  const eventTypeSet = new Set<string>();

  // Collect all event types
  for (const dep of deps) {
    for (const et of dep.inputEventTypes) eventTypeSet.add(et);
    for (const et of dep.outputEventTypes) eventTypeSet.add(et);
  }

  // Create event type nodes
  for (const et of eventTypeSet) {
    nodes.push({
      id: `et:${et}`,
      type: "event-type",
      position: { x: 0, y: 0 },
      data: { nodeKind: "event-type", eventType: et } as DepNodeData,
    });
  }

  // Create reactor nodes + edges
  for (const dep of deps) {
    const reactorNodeId = `r:${dep.reactorId}`;
    nodes.push({
      id: reactorNodeId,
      type: "reactor",
      position: { x: 0, y: 0 },
      data: { nodeKind: "reactor", reactorId: dep.reactorId } as DepNodeData,
    });

    for (const et of dep.inputEventTypes) {
      edges.push({
        id: `${et}→${dep.reactorId}`,
        source: `et:${et}`,
        target: reactorNodeId,
        markerEnd: { type: MarkerType.ArrowClosed, width: 12, height: 12, color: "#52525b" },
        style: { stroke: "#52525b", strokeWidth: 1.5 },
        animated: true,
      });
    }

    for (const et of dep.outputEventTypes) {
      edges.push({
        id: `${dep.reactorId}→${et}`,
        source: reactorNodeId,
        target: `et:${et}`,
        markerEnd: { type: MarkerType.ArrowClosed, width: 12, height: 12, color: "#3b82f6" },
        style: { stroke: "#3b82f6", strokeWidth: 1.5 },
      });
    }
  }

  // Layout with dagre
  const g = new dagre.graphlib.Graph();
  g.setDefaultEdgeLabel(() => ({}));
  g.setGraph({ rankdir: "LR", nodesep: 40, ranksep: 80 });

  for (const node of nodes) {
    const w = node.type === "reactor" ? REACTOR_NODE_W : EVENT_NODE_W;
    const h = node.type === "reactor" ? REACTOR_NODE_H : EVENT_NODE_H;
    g.setNode(node.id, { width: w, height: h });
  }
  for (const edge of edges) {
    g.setEdge(edge.source, edge.target);
  }
  dagre.layout(g);

  for (const node of nodes) {
    const pos = g.node(node.id);
    const w = node.type === "reactor" ? REACTOR_NODE_W : EVENT_NODE_W;
    const h = node.type === "reactor" ? REACTOR_NODE_H : EVENT_NODE_H;
    node.position = { x: pos.x - w / 2, y: pos.y - h / 2 };
  }

  return { nodes, edges };
}

// ---------------------------------------------------------------------------
// ReactorDependencyMapPane
// ---------------------------------------------------------------------------

export function ReactorDependencyMapPane() {
  const deps = useSelector<InspectorState, ReactorDependency[]>((s) => s.reactorDependencies);

  const { nodes, edges } = useMemo(() => buildDependencyGraph(deps), [deps]);

  const onInit = useCallback((instance: { fitView: () => void }) => {
    setTimeout(() => instance.fitView(), 50);
  }, []);

  if (deps.length === 0) {
    return (
      <div className="flex items-center justify-center h-full text-sm text-muted-foreground">
        No reactor dependencies found — submit some events to populate
      </div>
    );
  }

  return (
    <div className="h-full w-full">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        nodeTypes={nodeTypes}
        onInit={onInit}
        fitView
        nodesDraggable
        nodesConnectable={false}
        colorMode="dark"
      >
        <Background color="#27272a" gap={20} />
        <Controls showInteractive={false} />
      </ReactFlow>
    </div>
  );
}
