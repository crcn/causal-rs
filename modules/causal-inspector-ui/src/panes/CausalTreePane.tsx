import { useState, useMemo, useRef, useEffect, useCallback } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent, FlowSelection, ReactorOutcome, InspectorEffect } from "../types";
import { CopyablePayload } from "../components/CopyablePayload";
import { EffectList } from "../components/EffectList";
import { eventTextColor } from "../theme";
import { formatTs, compactPayload, copyToClipboard, inScrubberRange } from "../utils";
import { Copy, Check, Search, X, ChevronRight, ChevronDown, AlertTriangle, Zap } from "lucide-react";

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
  outcome,
  outcomesByReactor,
}: {
  reactorId: string;
  parentEventId: string;
  children: InspectorEvent[];
  childrenMap: Map<string, InspectorEvent[]>;
  depth: number;
  isHighlighted: boolean;
  onClickReactor: (reactorId: string, parentEventId: string) => void;
  outcome?: ReactorOutcome;
  outcomesByReactor?: Map<string, ReactorOutcome>;
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

  const isError = outcome?.status === "error";
  const isRunning = outcome?.status === "running";

  return (
    <div className={depth > 0 ? "pl-6" : ""}>
      <div
        ref={isHighlighted ? nodeRef : undefined}
        className={`group/tree w-full text-left px-2 py-1.5 rounded-md transition-all duration-150 hover:bg-white/[0.03] ${
          isHighlighted ? "bg-indigo-500/15 ring-1 ring-indigo-500/25" : ""
        } ${isError ? "bg-red-500/8" : ""}`}
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
            <span className={`px-1.5 py-0.5 rounded text-[9px] font-medium shrink-0 italic border ${
              isError
                ? "bg-red-500/10 text-red-400/80 border-red-500/20"
                : isRunning
                  ? "bg-yellow-500/10 text-yellow-400/80 border-yellow-500/20"
                  : "bg-white/[0.04] text-muted-foreground/60 border-border"
            }`}>
              reactor
            </span>
            <span className="text-[10px] font-mono text-foreground/60 shrink-0">
              {reactorId}
            </span>
            {isError && (
              <span className="flex items-center gap-1 text-[9px] text-red-400/80 shrink-0" title={outcome.error ?? "Error"}>
                <AlertTriangle size={10} />
                {outcome.attempts > 1 && <span>x{outcome.attempts}</span>}
              </span>
            )}
            {collapsed && (
              <span className="text-[10px] text-muted-foreground shrink-0">
                ({children.length})
              </span>
            )}
          </button>
        </div>
        {isError && outcome.error && (
          <div className="mt-1 ml-7 text-[9px] text-red-400/70 truncate" title={outcome.error}>
            {outcome.error}
          </div>
        )}
      </div>

      {!collapsed && children.map((child) => (
        <TreeNode
          key={child.seq}
          event={child}
          childrenMap={childrenMap}
          depth={depth + 1}
          onClickReactor={onClickReactor}
          outcomesByReactor={outcomesByReactor}
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
  outcomesByReactor,
}: {
  event: InspectorEvent;
  childrenMap: Map<string, InspectorEvent[]>;
  depth: number;
  onClickReactor: (reactorId: string, parentEventId: string) => void;
  onInvestigate?: (event: InspectorEvent) => void;
  outcomesByReactor?: Map<string, ReactorOutcome>;
}) {
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const flowSelection = useSelector<InspectorState, FlowSelection>((s) => s.flowSelection);
  const expandedEffects = useSelector<InspectorState, Record<string, InspectorEffect[]>>((s) => s.expandedEffects);
  const loadingEffectsIds = useSelector<InspectorState, string[]>((s) => s.loadingEffects);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const [payloadOpen, setPayloadOpen] = useState(false);
  const [effectsOpen, setEffectsOpen] = useState(false);
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
          if (event.workflowId) {
            dispatch({ type: "ui/flow_opened", payload: { workflowId: event.workflowId } });
          }
        }}
        className={`group/tree w-full text-left px-2 py-1.5 rounded-md transition-all duration-150 cursor-pointer hover:bg-white/[0.03] ${
          isSelected ? "bg-indigo-500/15 ring-1 ring-indigo-500/25" : ""
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
          {event.aggregateType && event.aggregateId && (
            <button
              onClick={(e) => {
                e.stopPropagation();
                dispatch({ type: "ui/subject_selected", payload: { aggregateType: event.aggregateType!, aggregateId: event.aggregateId!, mode: "both" } });
              }}
              className="px-1.5 py-0.5 rounded-full text-[9px] font-mono bg-teal-500/8 text-teal-400/80 hover:bg-teal-500/15 hover:text-teal-400 shrink-0 transition-all border border-teal-500/10"
              title={`View subject ${event.aggregateType}:${event.aggregateId}`}
            >
              {event.aggregateType}:{event.aggregateId.slice(0, 8)}
            </button>
          )}
          <span className="text-[10px] text-muted-foreground shrink-0">
            {formatTs(event.ts)}
          </span>
          {event.id && (
            <button
              onClick={(e) => {
                e.stopPropagation();
                if (!effectsOpen && expandedEffects[event.id!] === undefined) {
                  dispatch({ type: "ui/event_effects_requested", payload: { eventId: event.id! } });
                }
                setEffectsOpen((v) => !v);
              }}
              className={`opacity-0 group-hover/tree:opacity-100 transition-all duration-150 flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] ${
                effectsOpen
                  ? "opacity-100 bg-indigo-500/10 text-indigo-400/70 border border-indigo-500/20"
                  : "hover:bg-white/[0.05] text-muted-foreground/50 border border-transparent"
              }`}
              title="Show effects"
            >
              <Zap size={9} />
              {expandedEffects[event.id] !== undefined && expandedEffects[event.id].length > 0 && (
                <span>{expandedEffects[event.id].length}</span>
              )}
            </button>
          )}
          <button
            onClick={(e) => {
              e.stopPropagation();
              const json = buildTreeJson([event], childrenMap);
              const text = JSON.stringify(json[0], null, 2);
              copyToClipboard(text);
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            }}
            className="opacity-0 group-hover/tree:opacity-100 transition-all duration-150 ml-auto p-1 rounded-md hover:bg-white/[0.05] shrink-0 text-[10px] text-muted-foreground/50"
            title="Copy subtree as JSON"
          >
            {copied ? <Check size={12} /> : <Copy size={12} />}
          </button>
          {onInvestigate && (
            <button
              onClick={(e) => { e.stopPropagation(); onInvestigate(event); }}
              className="opacity-0 group-hover/tree:opacity-100 transition-all duration-150 p-1 rounded-md hover:bg-white/[0.05] shrink-0 text-muted-foreground/50"
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
        {effectsOpen && (
          <div className="mt-1 ml-3">
            {loadingEffectsIds.includes(event.id ?? "") ? (
              <div className="text-[9px] text-muted-foreground/40 italic py-1">Loading…</div>
            ) : (
              <EffectList effects={event.id ? (expandedEffects[event.id] ?? []) : []} />
            )}
          </div>
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
              outcomesByReactor={outcomesByReactor}
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
              outcome={outcomesByReactor?.get(hid)}
              outcomesByReactor={outcomesByReactor}
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
  if (sel.kind === "event-type") return event.name === sel.name;
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
  const flowWorkflowId = useSelector<InspectorState, string | null>((s) => s.flowWorkflowId);
  const scrubberStart = useSelector<InspectorState, number | null>((s) => s.scrubberStart);
  const scrubberEnd = useSelector<InspectorState, number | null>((s) => s.scrubberEnd);
  const outcomesMap = useSelector<InspectorState, Record<string, ReactorOutcome[]>>((s) => s.outcomes);
  const dispatch = useDispatch<InspectorMachineEvent>();

  // Build reactor outcome lookup for current flow
  const outcomesByReactor = useMemo(() => {
    if (!flowWorkflowId) return new Map<string, ReactorOutcome>();
    const raw = outcomesMap[flowWorkflowId];
    if (!raw) return new Map<string, ReactorOutcome>();
    const map = new Map<string, ReactorOutcome>();
    for (const o of raw) map.set(o.reactorId, o);
    return map;
  }, [outcomesMap, flowWorkflowId]);

  const treeEvents = useMemo(() => {
    const all = causalTree?.events ?? null;
    if (all == null || (scrubberStart == null && scrubberEnd == null)) return all;
    return all.filter((e) => inScrubberRange(e.seq, scrubberStart, scrubberEnd));
  }, [causalTree?.events, scrubberStart, scrubberEnd]);
  const treeLoading = selectedSeq != null && causalTree == null;

  const onClickReactor = useCallback(
    (reactorId: string, _parentEventId: string) => {
      if (flowWorkflowId) {
        dispatch({ type: "ui/flow_node_selected", payload: { kind: "reactor", reactorId } });
      }
      dispatch({ type: "ui/handler_selected", payload: { reactorId } });
    },
    [flowWorkflowId, dispatch]
  );

  const { roots, childrenMap, totalCount, filteredCount } = useMemo(() => {
    if (!treeEvents || treeEvents.length === 0)
      return { roots: [] as InspectorEvent[], childrenMap: new Map<string, InspectorEvent[]>(), totalCount: 0, filteredCount: 0 };

    const total = treeEvents.length;

    const events = (flowWorkflowId && flowSelection)
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
  }, [treeEvents, flowWorkflowId, flowSelection]);

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
      <div className="flex items-center justify-center h-full text-xs text-muted-foreground/50 tracking-wide">
        Select an event to view its causal tree
      </div>
    );
  }

  if (roots.length === 0 && flowSelection) {
    return (
      <div className="h-full overflow-y-auto p-3">
        <div className="flex items-center gap-2 mb-2 px-2.5 py-1.5 rounded-md bg-indigo-500/8 border border-indigo-500/15 text-xs text-indigo-400">
          <span>
            {flowSelection.kind === "event-type"
              ? flowSelection.name
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
        <div className="flex items-center gap-2 mb-2 px-2.5 py-1.5 rounded-md bg-indigo-500/8 border border-indigo-500/15 text-xs text-indigo-400">
          <span>
            {flowSelection.kind === "event-type"
              ? flowSelection.name
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
      <h3 className="text-[10px] font-semibold text-muted-foreground/50 mb-2 uppercase tracking-widest">
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
          outcomesByReactor={outcomesByReactor}
        />
      ))}
    </div>
  );
}
