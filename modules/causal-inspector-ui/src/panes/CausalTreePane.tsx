import { useState, useMemo, useRef, useEffect, useCallback } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent, FlowSelection } from "../types";
import { CopyablePayload } from "../components/CopyablePayload";
import { eventTextColor } from "../theme";
import { formatTs, compactPayload, copyToClipboard } from "../utils";
import { Copy, Check, Search, X, ChevronRight, ChevronDown } from "lucide-react";

// ---------------------------------------------------------------------------
// Tree JSON export
// ---------------------------------------------------------------------------

type TreeJson = {
  name: string;
  reactorId: string | null;
  summary: string | null;
  children?: TreeJson[];
};

function buildTreeJson(roots: InspectorEvent[], childrenMap: Map<string, InspectorEvent[]>): TreeJson[] {
  function toNode(evt: InspectorEvent): TreeJson {
    const children = evt.id ? (childrenMap.get(evt.id) ?? []) : [];
    const node: TreeJson = {
      name: evt.name,
      reactorId: evt.reactorId,
      summary: evt.summary,
    };
    if (children.length > 0) {
      node.children = children.map(toNode);
    }
    return node;
  }
  return roots.map(toNode);
}

// ---------------------------------------------------------------------------
// ReactorNode — intermediate node grouping children by reactor_id
// ---------------------------------------------------------------------------

function ReactorNode({
  reactorId,
  parentEventId,
  children,
  childrenMap,
  depth,
  isHighlighted,
  onClickReactor,
}: {
  reactorId: string;
  parentEventId: string;
  children: InspectorEvent[];
  childrenMap: Map<string, InspectorEvent[]>;
  depth: number;
  isHighlighted: boolean;
  onClickReactor: (reactorId: string, parentEventId: string) => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const nodeRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (isHighlighted && nodeRef.current) {
      nodeRef.current.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }, [isHighlighted]);

  const handleClick = useCallback(() => {
    onClickReactor(reactorId, parentEventId);
  }, [onClickReactor, parentEventId, reactorId]);

  return (
    <div className={depth > 0 ? "pl-6" : ""}>
      <div
        ref={isHighlighted ? nodeRef : undefined}
        className={`group/tree w-full text-left px-2 py-1 rounded transition-colors hover:bg-accent/30 ${
          isHighlighted ? "bg-zinc-700/40 ring-1 ring-zinc-500/50" : ""
        }`}
      >
        <div className="flex items-center gap-1.5 min-w-0">
          <button
            onClick={(e) => { e.stopPropagation(); setCollapsed(v => !v); }}
            className="text-[10px] text-muted-foreground hover:text-foreground shrink-0 w-3 text-center"
          >
            {collapsed ? <ChevronRight size={10} /> : <ChevronDown size={10} />}
          </button>
          <button
            onClick={handleClick}
            className="flex items-center gap-1.5 min-w-0"
          >
            <span className="px-1 py-0.5 rounded text-[10px] font-medium shrink-0 bg-zinc-600/30 text-zinc-400 italic">
              reactor
            </span>
            <span className="text-[10px] font-mono text-zinc-300 shrink-0">
              {reactorId}
            </span>
            {collapsed && (
              <span className="text-[10px] text-muted-foreground shrink-0">
                ({children.length})
              </span>
            )}
          </button>
        </div>
      </div>

      {!collapsed && children.map((child) => (
        <TreeNode
          key={child.seq}
          event={child}
          childrenMap={childrenMap}
          depth={depth + 1}
          onClickReactor={onClickReactor}
        />
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// TreeNode (recursive)
// ---------------------------------------------------------------------------

function TreeNode({
  event,
  childrenMap,
  depth,
  onClickReactor,
  onInvestigate,
}: {
  event: InspectorEvent;
  childrenMap: Map<string, InspectorEvent[]>;
  depth: number;
  onClickReactor: (reactorId: string, parentEventId: string) => void;
  onInvestigate?: (event: InspectorEvent) => void;
}) {
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const flowSelection = useSelector<InspectorState, FlowSelection>((s) => s.flowSelection);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const [payloadOpen, setPayloadOpen] = useState(false);
  const [collapsed, setCollapsed] = useState(false);
  const [copied, setCopied] = useState(false);
  const isSelected = event.seq === selectedSeq;
  const children = event.id ? (childrenMap.get(event.id) ?? []) : [];
  const hasChildren = children.length > 0;
  const nodeRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (isSelected && nodeRef.current) {
      nodeRef.current.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }, [isSelected]);

  // Group children by reactor_id
  const { reactorGroups, directChildren } = useMemo(() => {
    const groups = new Map<string, InspectorEvent[]>();
    const direct: InspectorEvent[] = [];
    for (const child of children) {
      if (child.reactorId) {
        const group = groups.get(child.reactorId) ?? [];
        group.push(child);
        groups.set(child.reactorId, group);
      } else {
        direct.push(child);
      }
    }
    return { reactorGroups: groups, directChildren: direct };
  }, [children]);

  const highlightedReactorId = flowSelection?.kind === "reactor" ? flowSelection.reactorId : null;

  return (
    <div className={depth > 0 ? "pl-6" : ""}>
      <div
        ref={isSelected ? nodeRef : undefined}
        onClick={() => {
          dispatch({ type: "ui/event_selected", payload: { seq: event.seq } });
          if (event.correlationId) {
            dispatch({ type: "ui/flow_opened", payload: { correlationId: event.correlationId } });
          }
        }}
        className={`group/tree w-full text-left px-2 py-1.5 rounded transition-colors cursor-pointer hover:bg-accent/30 ${
          isSelected ? "bg-accent/50 ring-1 ring-blue-500/50" : ""
        }`}
      >
        <div className="flex items-center gap-1.5 min-w-0">
          {hasChildren ? (
            <button
              onClick={(e) => { e.stopPropagation(); setCollapsed((v) => !v); }}
              className="text-[10px] text-muted-foreground hover:text-foreground shrink-0 w-3 text-center"
            >
              {collapsed ? <ChevronRight size={10} /> : <ChevronDown size={10} />}
            </button>
          ) : (
            <span className="w-3 shrink-0" />
          )}
          <span className="text-[10px] font-mono shrink-0" style={{ color: eventTextColor(event.name) }}>
            {event.name}
          </span>
          {collapsed && hasChildren && (
            <span className="text-[10px] text-muted-foreground shrink-0">
              ({children.length})
            </span>
          )}
          <span className="text-[10px] text-muted-foreground shrink-0">
            {formatTs(event.ts)}
          </span>
          <button
            onClick={(e) => {
              e.stopPropagation();
              const json = buildTreeJson([event], childrenMap);
              const text = JSON.stringify(json[0], null, 2);
              copyToClipboard(text);
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            }}
            className="opacity-0 group-hover/tree:opacity-100 transition-opacity ml-auto p-0.5 rounded hover:bg-accent shrink-0 text-[10px] text-muted-foreground"
            title="Copy subtree as JSON"
          >
            {copied ? <Check size={12} /> : <Copy size={12} />}
          </button>
          {onInvestigate && (
            <button
              onClick={(e) => { e.stopPropagation(); onInvestigate(event); }}
              className="opacity-0 group-hover/tree:opacity-100 transition-opacity p-0.5 rounded hover:bg-accent shrink-0 text-muted-foreground"
              title="Investigate"
            >
              <Search size={12} />
            </button>
          )}
        </div>
        <button
          onClick={(e) => { e.stopPropagation(); setPayloadOpen((v) => !v); }}
          className="mt-0.5 ml-3 text-[10px] font-mono text-muted-foreground hover:text-foreground truncate text-left max-w-full block"
          title="Click to expand payload"
        >
          {event.summary ?? compactPayload(event.payload)}
        </button>
        {payloadOpen && (
          <CopyablePayload payload={event.payload} className="mt-1 ml-3 max-h-48" />
        )}
      </div>

      {!collapsed && (
        <>
          {directChildren.map((child) => (
            <TreeNode
              key={child.seq}
              event={child}
              childrenMap={childrenMap}
              depth={depth + 1}
              onClickReactor={onClickReactor}
              onInvestigate={onInvestigate}
            />
          ))}
          {[...reactorGroups.entries()].map(([hid, group]) => (
            <ReactorNode
              key={hid}
              reactorId={hid}
              parentEventId={event.id!}
              children={group}
              childrenMap={childrenMap}
              depth={depth + 1}
              isHighlighted={hid === highlightedReactorId}
              onClickReactor={onClickReactor}
            />
          ))}
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// matchesFlowSelection
// ---------------------------------------------------------------------------

function matchesFlowSelection(event: InspectorEvent, sel: FlowSelection): boolean {
  if (!sel) return true;
  if (sel.kind === "event-type")
    return event.reactorId === sel.reactorId && event.name === sel.name;
  return event.reactorId === sel.reactorId;
}

// ---------------------------------------------------------------------------
// CausalTreePane
// ---------------------------------------------------------------------------

export type CausalTreePaneProps = {
  onInvestigate?: (event: InspectorEvent) => void;
};

export function CausalTreePane({ onInvestigate }: CausalTreePaneProps = {}) {
  const causalTree = useSelector<InspectorState, InspectorState["causalTree"]>((s) => s.causalTree);
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const flowSelection = useSelector<InspectorState, FlowSelection>((s) => s.flowSelection);
  const flowCorrelationId = useSelector<InspectorState, string | null>((s) => s.flowCorrelationId);
  const scrubberPosition = useSelector<InspectorState, number | null>((s) => s.scrubberPosition);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const treeEvents = useMemo(() => {
    const all = causalTree?.events ?? null;
    if (all == null || scrubberPosition == null) return all;
    return all.filter((e) => e.seq <= scrubberPosition);
  }, [causalTree?.events, scrubberPosition]);
  const treeLoading = selectedSeq != null && causalTree == null;

  const onClickReactor = useCallback(
    (reactorId: string, parentEventId: string) => {
      if (flowCorrelationId) {
        dispatch({ type: "ui/flow_node_selected", payload: { kind: "reactor", reactorId } });
      }
      dispatch({
        type: "ui/logs_filter_changed",
        payload: { eventId: parentEventId, reactorId, scope: "reactor" },
      });
    },
    [flowCorrelationId, dispatch]
  );

  const { roots, childrenMap, totalCount, filteredCount } = useMemo(() => {
    if (!treeEvents || treeEvents.length === 0)
      return { roots: [] as InspectorEvent[], childrenMap: new Map<string, InspectorEvent[]>(), totalCount: 0, filteredCount: 0 };

    const total = treeEvents.length;

    const events = (flowCorrelationId && flowSelection)
      ? treeEvents.filter(e => matchesFlowSelection(e, flowSelection))
      : treeEvents;

    const idSet = new Set(events.map(e => e.id).filter(Boolean));
    const cMap = new Map<string, InspectorEvent[]>();
    const rootList: InspectorEvent[] = [];

    for (const evt of events) {
      if (evt.parentId == null || !idSet.has(evt.parentId)) {
        rootList.push(evt);
      } else {
        const siblings = cMap.get(evt.parentId) ?? [];
        siblings.push(evt);
        cMap.set(evt.parentId, siblings);
      }
    }

    rootList.sort((a, b) => a.seq - b.seq);
    const filtered = rootList.length + [...cMap.values()].reduce((s, a) => s + a.length, 0);
    return { roots: rootList, childrenMap: cMap, totalCount: total, filteredCount: filtered };
  }, [treeEvents, flowCorrelationId, flowSelection]);

  if (treeLoading) {
    return (
      <div className="p-3 space-y-1.5 animate-pulse">
        <div className="h-3 w-32 bg-muted rounded mb-3" />
        <div className="flex items-center gap-1.5">
          <div className="h-4 w-12 bg-muted rounded" />
          <div className="h-4 w-36 bg-muted rounded" />
          <div className="h-3 w-24 bg-muted rounded" />
        </div>
        <div className="pl-6 space-y-1.5">
          <div className="flex items-center gap-1.5">
            <div className="h-4 w-14 bg-muted rounded" />
            <div className="h-4 w-44 bg-muted rounded" />
            <div className="h-3 w-24 bg-muted rounded" />
          </div>
          <div className="flex items-center gap-1.5">
            <div className="h-4 w-10 bg-muted rounded" />
            <div className="h-4 w-32 bg-muted rounded" />
            <div className="h-3 w-24 bg-muted rounded" />
          </div>
        </div>
      </div>
    );
  }

  if (!treeEvents) {
    return (
      <div className="flex items-center justify-center h-full text-sm text-muted-foreground">
        Select an event to view its causal tree
      </div>
    );
  }

  if (roots.length === 0 && flowSelection) {
    return (
      <div className="h-full overflow-y-auto p-3">
        <div className="flex items-center gap-2 mb-2 px-2 py-1 rounded bg-blue-500/10 text-xs text-blue-400">
          <span>
            {flowSelection.kind === "event-type"
              ? `${flowSelection.name} from ${flowSelection.reactorId ?? "root"}`
              : `outputs of ${flowSelection.reactorId}`}
          </span>
          <button
            onClick={() => dispatch({ type: "ui/flow_node_selected", payload: null })}
            className="ml-auto hover:text-foreground"
          >
            <X size={12} />
          </button>
        </div>
        <div className="flex items-center justify-center h-32 text-sm text-muted-foreground">
          No events match the current filter
        </div>
      </div>
    );
  }

  return (
    <div className="h-full overflow-y-auto p-3">
      {flowSelection && (
        <div className="flex items-center gap-2 mb-2 px-2 py-1 rounded bg-blue-500/10 text-xs text-blue-400">
          <span>
            {flowSelection.kind === "event-type"
              ? `${flowSelection.name} from ${flowSelection.reactorId ?? "root"}`
              : `outputs of ${flowSelection.reactorId}`}
          </span>
          <button
            onClick={() => dispatch({ type: "ui/flow_node_selected", payload: null })}
            className="ml-auto hover:text-foreground"
          >
            <X size={12} />
          </button>
        </div>
      )}
      <h3 className="text-xs font-semibold text-muted-foreground mb-2 uppercase tracking-wider">
        Causal Tree ({flowSelection ? `${filteredCount} of ${totalCount}` : totalCount} events)
      </h3>
      {roots.map(root => (
        <TreeNode
          key={root.seq}
          event={root}
          childrenMap={childrenMap}
          depth={0}
          onClickReactor={onClickReactor}
          onInvestigate={onInvestigate}
        />
      ))}
    </div>
  );
}
