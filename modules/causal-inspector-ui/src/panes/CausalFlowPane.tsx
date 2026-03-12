import { useMemo, useCallback, useEffect, useRef, useState, memo } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  useReactFlow,
  Handle,
  MarkerType,
  type Node,
  type Edge,
  type NodeChange,
  type NodeProps,
  Position,
} from "@xyflow/react";
import dagre from "@dagrejs/dagre";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent, Block, FlowSelection, ReactorDescription, ReactorOutcome } from "../types";
import { eventBg, eventBorder } from "../theme";
import { Filter, Search } from "lucide-react";

// ---------------------------------------------------------------------------
// Flow node data
// ---------------------------------------------------------------------------

type FlowNodeData =
  | { nodeKind: "event-type"; reactorId: string | null; eventName: string; label: string }
  | { nodeKind: "reactor"; reactorId: string; label: string; blocks?: Block[]; outcome?: ReactorOutcome };

/* eslint-disable-next-line @typescript-eslint/no-redeclare -- shadowing the tree-pane ReactorNode on purpose */

const NODE_WIDTH = 200;
const NODE_HEIGHT = 50;
const REACTOR_WIDTH = 180;
const REACTOR_HEIGHT = 36;

// ---------------------------------------------------------------------------
// Block renderers
// ---------------------------------------------------------------------------

function BlockRenderer({ block }: { block: Block }) {
  switch (block.type) {
    case "checklist":
      return (
        <div style={{ marginTop: 4 }}>
          <div style={{ fontSize: 9, color: "#71717a", marginBottom: 2 }}>{block.label}</div>
          {block.items.map((item, i) => (
            <div key={i} style={{ fontSize: 9, color: item.done ? "#22c55e" : "#52525b", display: "flex", gap: 3, alignItems: "center" }}>
              <span>{item.done ? "\u2713" : "\u25cb"}</span>
              <span>{item.text}</span>
            </div>
          ))}
        </div>
      );
    case "counter":
      return (
        <div style={{ fontSize: 9, color: "#a1a1aa", marginTop: 2 }}>
          {block.label}: {block.value}/{block.total}
        </div>
      );
    case "progress": {
      const pct = Math.round(block.fraction * 100);
      return (
        <div style={{ marginTop: 2 }}>
          <div style={{ fontSize: 9, color: "#a1a1aa" }}>{block.label}: {pct}%</div>
          <div style={{ height: 3, background: "#3f3f46", borderRadius: 2, marginTop: 1 }}>
            <div style={{ height: "100%", width: `${pct}%`, background: "#22c55e", borderRadius: 2 }} />
          </div>
        </div>
      );
    }
    case "label":
      return <div style={{ fontSize: 9, color: "#a1a1aa", marginTop: 2 }}>{block.text}</div>;
    case "key_value":
      return (
        <div style={{ fontSize: 9, color: "#a1a1aa", marginTop: 2 }}>
          <span style={{ color: "#71717a" }}>{block.key}:</span> {block.value}
        </div>
      );
    case "status": {
      const colors: Record<string, string> = { waiting: "#71717a", running: "#eab308", done: "#22c55e", error: "#ef4444" };
      return (
        <div style={{ fontSize: 9, color: colors[block.state] ?? "#a1a1aa", marginTop: 2 }}>
          {block.label}: {block.state}
        </div>
      );
    }
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// Custom nodes
// ---------------------------------------------------------------------------

function formatDuration(startedAt: string, completedAt: string): string {
  const ms = new Date(completedAt).getTime() - new Date(startedAt).getTime();
  if (ms < 1000) return `${ms}ms`;
  const secs = ms / 1000;
  if (secs < 60) return `${secs.toFixed(1)}s`;
  const mins = Math.floor(secs / 60);
  const remainSecs = Math.round(secs % 60);
  return remainSecs > 0 ? `${mins}m ${remainSecs}s` : `${mins}m`;
}

const STATUS_BORDER: Record<string, string> = {
  pending: "#52525b",
  running: "#eab308",
  completed: "#22c55e",
  error: "#ef4444",
};

const ReactorNode = memo(({ data }: NodeProps) => {
  const d = data as FlowNodeData & { nodeKind: "reactor" };
  const blocks = d.blocks;
  const outcome = d.outcome;
  const hasBlocks = blocks && blocks.length > 0;
  const borderColor = STATUS_BORDER[outcome?.status ?? "pending"] ?? "#52525b";
  const isRunning = outcome?.status === "running";
  const duration = outcome?.status === "completed" && outcome.startedAt && outcome.completedAt
    ? formatDuration(outcome.startedAt, outcome.completedAt)
    : null;

  return (
    <div style={{
      background: "#27272a",
      border: `1px solid ${borderColor}`,
      borderRadius: hasBlocks ? 8 : 20,
      fontSize: 10,
      padding: hasBlocks ? "6px 10px" : "4px 12px",
      width: REACTOR_WIDTH,
      color: "#a1a1aa",
      fontStyle: "italic",
      animation: isRunning ? "pulse 2s ease-in-out infinite" : undefined,
    }}>
      <Handle type="target" position={Position.Top} style={{ visibility: "hidden" }} />
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 8 }}>
        <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", direction: "rtl", textAlign: "left" }}>{d.label}</span>
        {duration && <span style={{ fontSize: 9, color: "#71717a", fontStyle: "normal", whiteSpace: "nowrap", flexShrink: 0 }}>{duration}</span>}
      </div>
      {hasBlocks && blocks.map((block, i) => <BlockRenderer key={i} block={block} />)}
      {outcome?.status === "error" && outcome.error && (
        <div style={{ fontSize: 9, color: "#ef4444", marginTop: 4 }}>{outcome.error}</div>
      )}
      <Handle type="source" position={Position.Bottom} style={{ visibility: "hidden" }} />
    </div>
  );
});

const EventNode = memo(({ data }: NodeProps) => {
  const d = data as FlowNodeData & { nodeKind: "event-type" };
  return (
    <div style={{
      background: eventBg(d.eventName),
      border: `1px solid ${eventBorder(d.eventName)}`,
      borderRadius: 6,
      fontSize: 11,
      padding: "6px 10px",
      width: NODE_WIDTH,
      color: "#e4e4e7",
      overflow: "hidden",
      textOverflow: "ellipsis",
      whiteSpace: "nowrap",
      direction: "rtl",
      textAlign: "left",
    }}>
      <Handle type="target" position={Position.Top} style={{ visibility: "hidden" }} />
      {d.label}
      <Handle type="source" position={Position.Bottom} style={{ visibility: "hidden" }} />
    </div>
  );
});

const nodeTypes = { reactor: ReactorNode, event: EventNode };

// ---------------------------------------------------------------------------
// Graph building
// ---------------------------------------------------------------------------

type FlowGraph = { nodes: Node[]; edges: Edge[] };

function buildFlowGraph(
  events: InspectorEvent[],
  descriptions?: Map<string, Block[]>,
  outcomes?: Map<string, ReactorOutcome>,
  hiddenReactors?: Set<string>,
): FlowGraph {
  const eventGroups = new Map<string, { name: string; count: number; events: InspectorEvent[] }>();
  const reactorIds = new Set<string>();
  const parentToReactor = new Map<string, Set<string>>();
  const reactorToChildren = new Map<string, Set<string>>();

  for (const evt of events) {
    const reactor = evt.reactorId ?? "__root__";
    const groupKey = `${reactor}::${evt.name}`;

    const group = eventGroups.get(groupKey);
    if (group) {
      group.count++;
      group.events.push(evt);
    } else {
      eventGroups.set(groupKey, { name: evt.name, count: 1, events: [evt] });
    }

    if (evt.reactorId) {
      reactorIds.add(evt.reactorId);
      const children = reactorToChildren.get(evt.reactorId) ?? new Set();
      children.add(groupKey);
      reactorToChildren.set(evt.reactorId, children);
    }

    if (evt.parentId && evt.reactorId) {
      const reactors = parentToReactor.get(evt.parentId) ?? new Set();
      reactors.add(evt.reactorId);
      parentToReactor.set(evt.parentId, reactors);
    }
  }

  const eventIdToGroup = new Map<string, string>();
  for (const [groupKey, group] of eventGroups) {
    for (const evt of group.events) {
      if (evt.id) eventIdToGroup.set(evt.id, groupKey);
    }
  }

  const nodes: Node[] = [];
  const edges: Edge[] = [];
  const edgeSet = new Set<string>();

  // Event-type nodes
  for (const [groupKey, group] of eventGroups) {
    const emittingReactor = group.events[0]?.reactorId;
    if (emittingReactor && hiddenReactors?.has(emittingReactor)) continue;
    nodes.push({
      id: `evt:${groupKey}`,
      type: "event",
      position: { x: 0, y: 0 },
      data: {
        label: `${group.name} (${group.count})`,
        nodeKind: "event-type" as const,
        reactorId: group.events[0]?.reactorId ?? null,
        eventName: group.name,
      },
      sourcePosition: Position.Bottom,
      targetPosition: Position.Top,
    });
  }

  // Reactor nodes
  for (const reactorId of reactorIds) {
    if (hiddenReactors?.has(reactorId)) continue;
    const blocks = descriptions?.get(reactorId);
    const outcome = outcomes?.get(reactorId);
    nodes.push({
      id: `hdl:${reactorId}`,
      type: "reactor",
      position: { x: 0, y: 0 },
      data: { label: reactorId, nodeKind: "reactor" as const, reactorId, blocks, outcome },
      sourcePosition: Position.Bottom,
      targetPosition: Position.Top,
    });
  }

  const arrowMarker = { type: MarkerType.ArrowClosed, color: "#52525b", width: 16, height: 16 };

  // Edges: event group -> reactor
  for (const [parentId, reactors] of parentToReactor) {
    const sourceGroupKey = eventIdToGroup.get(parentId);
    if (!sourceGroupKey) continue;
    for (const reactorId of reactors) {
      if (hiddenReactors?.has(reactorId)) continue;
      const edgeKey = `evt:${sourceGroupKey}->hdl:${reactorId}`;
      if (!edgeSet.has(edgeKey)) {
        edgeSet.add(edgeKey);
        edges.push({
          id: edgeKey,
          source: `evt:${sourceGroupKey}`,
          target: `hdl:${reactorId}`,
          style: { stroke: "#52525b", strokeWidth: 1 },
          markerEnd: arrowMarker,
        });
      }
    }
  }

  // Edges: reactor -> child event groups
  for (const [reactorId, childGroupKeys] of reactorToChildren) {
    if (hiddenReactors?.has(reactorId)) continue;
    for (const groupKey of childGroupKeys) {
      const edgeKey = `hdl:${reactorId}->evt:${groupKey}`;
      if (!edgeSet.has(edgeKey)) {
        edgeSet.add(edgeKey);
        const count = eventGroups.get(groupKey)?.count ?? 0;
        edges.push({
          id: edgeKey,
          source: `hdl:${reactorId}`,
          target: `evt:${groupKey}`,
          style: { stroke: "#52525b", strokeWidth: 1 },
          markerEnd: arrowMarker,
          ...(count > 1 ? { label: `x${count}`, labelStyle: { fontSize: 9, fill: "#71717a" } } : {}),
        });
      }
    }
  }

  // Root events -> reactor edges
  for (const [groupKey, group] of eventGroups) {
    if (group.events[0]?.reactorId) continue;
    for (const evt of group.events) {
      if (!evt.id) continue;
      const reactors = parentToReactor.get(evt.id);
      if (!reactors) continue;
      for (const reactorId of reactors) {
        if (hiddenReactors?.has(reactorId)) continue;
        const edgeKey = `evt:${groupKey}->hdl:${reactorId}`;
        if (!edgeSet.has(edgeKey)) {
          edgeSet.add(edgeKey);
          edges.push({
            id: edgeKey,
            source: `evt:${groupKey}`,
            target: `hdl:${reactorId}`,
            style: { stroke: "#52525b", strokeWidth: 1 },
            markerEnd: arrowMarker,
          });
        }
      }
    }
  }

  // Reactors known from outcomes but not from event stream
  if (outcomes) {
    for (const [reactorId, outcome] of outcomes) {
      if (reactorIds.has(reactorId)) continue;
      if (hiddenReactors?.has(reactorId)) continue;

      const blocks = descriptions?.get(reactorId);
      nodes.push({
        id: `hdl:${reactorId}`,
        type: "reactor",
        position: { x: 0, y: 0 },
        data: { label: reactorId, nodeKind: "reactor" as const, reactorId, blocks, outcome },
        sourcePosition: Position.Bottom,
        targetPosition: Position.Top,
      });

      const isPending = outcome.status === "pending" || outcome.status === "running";
      for (const eventId of outcome.triggeringEventIds ?? []) {
        const groupKey = eventIdToGroup.get(eventId);
        if (!groupKey) continue;
        const edgeKey = `evt:${groupKey}->hdl:${reactorId}`;
        if (!edgeSet.has(edgeKey)) {
          edgeSet.add(edgeKey);
          edges.push({
            id: edgeKey,
            source: `evt:${groupKey}`,
            target: `hdl:${reactorId}`,
            style: { stroke: "#52525b", strokeWidth: 1 },
            markerEnd: arrowMarker,
            animated: isPending,
          });
        }
      }
    }
  }

  return layoutGraph(nodes, edges);
}

function estimateReactorHeight(data: FlowNodeData): number {
  if (data.nodeKind !== "reactor") return REACTOR_HEIGHT;
  const hasBlocks = data.blocks && data.blocks.length > 0;
  const outcome = data.outcome;
  if (!hasBlocks && !outcome) return REACTOR_HEIGHT;
  let h = 24;
  if (data.blocks) {
    for (const block of data.blocks) {
      if (block.type === "checklist") {
        h += 14 + block.items.length * 12;
      } else {
        h += 14;
      }
    }
  }
  if (outcome?.status === "error" && outcome.error) h += 14;
  return h;
}

function layoutGraph(nodes: Node[], edges: Edge[]): FlowGraph {
  const g = new dagre.graphlib.Graph();
  g.setDefaultEdgeLabel(() => ({}));
  g.setGraph({ rankdir: "TB", ranksep: 60, nodesep: 30 });

  const heights = new Map<string, number>();
  for (const node of nodes) {
    const isReactor = node.id.startsWith("hdl:");
    const h = isReactor ? estimateReactorHeight(node.data as FlowNodeData) : NODE_HEIGHT;
    heights.set(node.id, h);
    g.setNode(node.id, {
      width: isReactor ? REACTOR_WIDTH : NODE_WIDTH,
      height: h,
    });
  }

  for (const edge of edges) {
    g.setEdge(edge.source, edge.target);
  }

  dagre.layout(g);

  const laidOut = nodes.map((node) => {
    const pos = g.node(node.id);
    const isReactor = node.id.startsWith("hdl:");
    const w = isReactor ? REACTOR_WIDTH : NODE_WIDTH;
    const h = heights.get(node.id) ?? NODE_HEIGHT;
    return {
      ...node,
      position: { x: pos.x - w / 2, y: pos.y - h / 2 },
    };
  });

  return { nodes: laidOut, edges };
}

// ---------------------------------------------------------------------------
// Auto-center on selection
// ---------------------------------------------------------------------------

function FocusOnSelection({ nodes, flowData }: { nodes: Node[]; flowData: InspectorEvent[] }) {
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const { setCenter, getZoom } = useReactFlow();

  useEffect(() => {
    if (selectedSeq == null || !flowData.length) return;
    const evt = flowData.find(e => e.seq === selectedSeq);
    if (!evt) return;

    const reactor = evt.reactorId ?? "__root__";
    const nodeId = `evt:${reactor}::${evt.name}`;
    const node = nodes.find(n => n.id === nodeId);
    if (!node) return;

    const isReactor = node.id.startsWith("hdl:");
    const w = isReactor ? REACTOR_WIDTH : NODE_WIDTH;
    const h = isReactor ? estimateReactorHeight(node.data as FlowNodeData) : NODE_HEIGHT;

    setCenter(
      node.position.x + w / 2,
      node.position.y + h / 2,
      { zoom: getZoom(), duration: 400 },
    );
  }, [selectedSeq, flowData, nodes, setCenter, getZoom]);

  return null;
}

// ---------------------------------------------------------------------------
// Reactor filter dropdown
// ---------------------------------------------------------------------------

function ReactorFilter({ allReactorIds, hiddenReactors, setHiddenReactors }: {
  allReactorIds: string[];
  hiddenReactors: Set<string>;
  setHiddenReactors: (s: Set<string>) => void;
}) {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const containerRef = useRef<HTMLDivElement>(null);

  // Close on click outside
  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as globalThis.Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [open]);

  const toggle = (id: string) => {
    const next = new Set(hiddenReactors);
    if (next.has(id)) next.delete(id); else next.add(id);
    setHiddenReactors(next);
  };

  const filtered = filter
    ? allReactorIds.filter(id => id.toLowerCase().includes(filter.toLowerCase()))
    : allReactorIds;

  const hiddenCount = hiddenReactors.size;

  return (
    <div ref={containerRef} className="relative">
      <button
        onClick={() => setOpen(v => !v)}
        className="text-xs text-muted-foreground hover:text-foreground px-1.5 py-0.5 rounded border border-border"
      >
        <Filter size={12} className="inline mr-1" />
        {hiddenCount > 0 ? `${hiddenCount} hidden` : "Filter"}
      </button>
      {open && (
        <div className="absolute top-full left-0 mt-1 z-50 bg-background border border-border rounded-md shadow-lg min-w-[240px]">
          <div className="px-2 py-1.5 border-b border-border">
            <input
              autoFocus
              type="text"
              value={filter}
              onChange={e => setFilter(e.target.value)}
              placeholder="Search reactors..."
              className="w-full text-xs bg-transparent border-none outline-none text-foreground placeholder:text-muted-foreground"
            />
          </div>
          <div className="max-h-64 overflow-y-auto py-1">
            {filtered.map(id => (
              <label key={id} className="flex items-center gap-2 px-3 py-1 hover:bg-accent cursor-pointer">
                <input
                  type="checkbox"
                  checked={!hiddenReactors.has(id)}
                  onChange={() => toggle(id)}
                  className="rounded border-border"
                />
                <span className="text-xs font-mono text-foreground truncate">{id}</span>
              </label>
            ))}
            {filtered.length === 0 && (
              <div className="text-xs text-muted-foreground px-3 py-2">No matches</div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// CausalFlowPane
// ---------------------------------------------------------------------------

export type CausalFlowPaneProps = {
  /** Optional set of reactor IDs to hide by default. */
  defaultHiddenReactors?: Set<string>;
  /** Optional extra header content (e.g., domain-specific run stats). */
  headerExtra?: React.ReactNode;
};

export function CausalFlowPane({ defaultHiddenReactors, headerExtra }: CausalFlowPaneProps = {}) {
  const flowCorrelationId = useSelector<InspectorState, string | null>((s) => s.flowCorrelationId);
  const flowData = useSelector<InspectorState, InspectorEvent[]>((s) => s.flowData);
  const flowSelection = useSelector<InspectorState, FlowSelection>((s) => s.flowSelection);
  const descriptionsMap = useSelector<InspectorState, Record<string, ReactorDescription[]>>((s) => s.descriptions);
  const outcomesMap = useSelector<InspectorState, Record<string, ReactorOutcome[]>>((s) => s.outcomes);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const flowLoading = flowCorrelationId != null && flowData.length === 0;

  // Build typed maps from state
  const descriptions = useMemo(() => {
    if (!flowCorrelationId) return undefined;
    const raw = descriptionsMap[flowCorrelationId];
    if (!raw) return undefined;
    const map = new Map<string, Block[]>();
    for (const d of raw) map.set(d.reactorId, d.blocks);
    return map;
  }, [descriptionsMap, flowCorrelationId]);

  const outcomes = useMemo(() => {
    if (!flowCorrelationId) return undefined;
    const raw = outcomesMap[flowCorrelationId];
    if (!raw) return undefined;
    const map = new Map<string, ReactorOutcome>();
    for (const o of raw) map.set(o.reactorId, o);
    return map;
  }, [outcomesMap, flowCorrelationId]);

  const [hiddenReactors, setHiddenReactors] = useState<Set<string>>(
    () => defaultHiddenReactors ?? new Set()
  );

  const allReactorIds = useMemo(() => {
    const ids = new Set<string>();
    for (const evt of flowData) { if (evt.reactorId) ids.add(evt.reactorId); }
    if (outcomes) for (const id of outcomes.keys()) ids.add(id);
    return [...ids].sort();
  }, [flowData, outcomes]);

  const { nodes: rawNodes, edges: rawEdges } = useMemo(() => {
    if (!flowData || flowData.length === 0) return { nodes: [], edges: [] };
    return buildFlowGraph(flowData, descriptions, outcomes, hiddenReactors);
  }, [flowData, descriptions, outcomes, hiddenReactors]);

  // Derive selected node ID from flowSelection
  const selectedNodeId = useMemo(() => {
    if (!flowSelection) return null;
    if (flowSelection.kind === "reactor") return `hdl:${flowSelection.reactorId}`;
    const reactor = flowSelection.reactorId ?? "__root__";
    return `evt:${reactor}::${flowSelection.name}`;
  }, [flowSelection]);

  // Walk causal chain for highlighting
  const causalNodeIds = useMemo(() => {
    if (!selectedNodeId) return null;

    const forward = new Map<string, string[]>();
    const backward = new Map<string, string[]>();
    for (const e of rawEdges) {
      forward.set(e.source, [...(forward.get(e.source) ?? []), e.target]);
      backward.set(e.target, [...(backward.get(e.target) ?? []), e.source]);
    }

    const visited = new Set<string>();
    const walk = (id: string, adj: Map<string, string[]>) => {
      if (visited.has(id)) return;
      visited.add(id);
      for (const next of adj.get(id) ?? []) walk(next, adj);
    };

    walk(selectedNodeId, forward);
    walk(selectedNodeId, backward);
    return visited;
  }, [selectedNodeId, rawEdges]);

  const nodes = useMemo(
    () => rawNodes.map(n => ({
      ...n,
      selected: n.id === selectedNodeId,
      style: {
        ...n.style,
        ...(causalNodeIds != null && !causalNodeIds.has(n.id) ? { opacity: 0.5 } : {}),
      },
    })),
    [rawNodes, selectedNodeId, causalNodeIds],
  );

  const edges = useMemo(
    () => rawEdges.map(e => {
      const base = { ...e, zIndex: -1 };
      if (!causalNodeIds) return base;
      const onPath = causalNodeIds.has(e.source) && causalNodeIds.has(e.target);
      return {
        ...base,
        style: {
          ...e.style,
          stroke: onPath ? "#e879f9" : "#52525b",
          strokeWidth: onPath ? 2 : 1,
          opacity: onPath ? 1 : 0.15,
        },
        markerEnd: onPath
          ? { type: MarkerType.ArrowClosed, color: "#e879f9", width: 16, height: 16 }
          : e.markerEnd,
      };
    }),
    [rawEdges, causalNodeIds],
  );

  const syncTree = useCallback((d: FlowNodeData) => {
    if (!flowData.length) return;
    const match = d.nodeKind === "event-type"
      ? flowData.find(e => e.reactorId === d.reactorId && e.name === d.eventName)
      : flowData.find(e => e.reactorId === d.reactorId);
    if (match) {
      dispatch({ type: "ui/event_selected", payload: { seq: match.seq } });
    }
  }, [flowData, dispatch]);

  const openLogsForReactor = useCallback((reactorId: string) => {
    const evt = flowData.find(e => e.reactorId === reactorId && e.parentId);
    if (evt) {
      dispatch({
        type: "ui/logs_filter_changed",
        payload: { eventId: evt.parentId!, reactorId, correlationId: evt.correlationId, scope: "reactor" },
      });
    }
  }, [flowData, dispatch]);

  const onNodeClick = useCallback((_event: React.MouseEvent, node: Node) => {
    const d = node.data as FlowNodeData;

    if (d.nodeKind === "event-type") {
      if (
        flowSelection?.kind === "event-type" &&
        flowSelection.reactorId === d.reactorId &&
        flowSelection.name === d.eventName
      ) {
        dispatch({ type: "ui/flow_node_selected", payload: null });
      } else {
        dispatch({
          type: "ui/flow_node_selected",
          payload: { kind: "event-type", reactorId: d.reactorId, name: d.eventName },
        });
        syncTree(d);
      }
    } else if (d.nodeKind === "reactor") {
      if (flowSelection?.kind === "reactor" && flowSelection.reactorId === d.reactorId) {
        dispatch({ type: "ui/flow_node_selected", payload: null });
      } else {
        dispatch({
          type: "ui/flow_node_selected",
          payload: { kind: "reactor", reactorId: d.reactorId },
        });
        syncTree(d);
        openLogsForReactor(d.reactorId);
      }
    }
  }, [flowSelection, dispatch, syncTree, openLogsForReactor]);

  const onPaneClick = useCallback(() => {
    dispatch({ type: "ui/flow_node_selected", payload: null });
  }, [dispatch]);

  const onNodesChange = useCallback((_changes: NodeChange[]) => {}, []);

  if (!flowCorrelationId) {
    return (
      <div className="flex items-center justify-center h-full text-sm text-muted-foreground">
        Select an event to visualize its causal flow
      </div>
    );
  }

  if (flowLoading) {
    return (
      <div className="h-full flex flex-col">
        <div className="flex items-center gap-2 px-3 py-1.5 border-b border-border shrink-0">
          <div className="h-3 w-10 bg-muted rounded animate-pulse" />
          <div className="h-3 w-48 bg-muted rounded animate-pulse" />
        </div>
        <div className="flex-1 flex items-center justify-center">
          <div className="animate-pulse flex flex-col items-center gap-3">
            <div className="h-8 w-40 bg-muted rounded-md" />
            <div className="h-6 w-px bg-muted" />
            <div className="h-6 w-28 bg-muted rounded-full" />
            <div className="flex items-start gap-8">
              <div className="flex flex-col items-center gap-3">
                <div className="h-6 w-px bg-muted" />
                <div className="h-8 w-36 bg-muted rounded-md" />
              </div>
              <div className="flex flex-col items-center gap-3">
                <div className="h-6 w-px bg-muted" />
                <div className="h-8 w-36 bg-muted rounded-md" />
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="h-full flex flex-col">
      <div className="flex items-center gap-2 px-3 py-1.5 border-b border-border shrink-0">
        <h3 className="text-xs font-semibold text-muted-foreground uppercase tracking-wider">
          Flow
        </h3>
        <span className="text-xs font-mono text-foreground truncate">{flowCorrelationId}</span>
        <span className="text-xs text-muted-foreground">
          {flowData.length} events, {nodes.length} nodes
        </span>
        {headerExtra}
        <ReactorFilter
          allReactorIds={allReactorIds}
          hiddenReactors={hiddenReactors}
          setHiddenReactors={setHiddenReactors}
        />
      </div>
      <div className="flex-1">
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          onNodesChange={onNodesChange}
          onNodeClick={onNodeClick}
          onPaneClick={onPaneClick}
          fitView
          minZoom={0.25}
          proOptions={{ hideAttribution: true }}
          nodesDraggable={false}
          nodesConnectable={false}
          elevateNodesOnSelect={false}
          colorMode="dark"
        >
          <FocusOnSelection nodes={nodes} flowData={flowData} />
          <Background color="#27272a" gap={20} />
          <Controls showInteractive={false} />
        </ReactFlow>
      </div>
    </div>
  );
}
