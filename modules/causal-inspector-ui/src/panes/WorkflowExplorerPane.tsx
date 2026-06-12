import { useState, useCallback, useEffect, useRef } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { WorkflowSummary } from "../types";
import { eventTextColor, eventBg, eventBorder } from "../theme";
import { formatTs } from "../utils";

function RelativeDuration({ firstTs, lastTs }: { firstTs: string; lastTs: string }) {
  const first = new Date(firstTs).getTime();
  const last = new Date(lastTs).getTime();
  const diffMs = last - first;

  if (diffMs < 1000) return <span>{diffMs}ms</span>;
  if (diffMs < 60_000) return <span>{(diffMs / 1000).toFixed(1)}s</span>;
  if (diffMs < 3_600_000) return <span>{(diffMs / 60_000).toFixed(1)}m</span>;
  return <span>{(diffMs / 3_600_000).toFixed(1)}h</span>;
}

export type WorkflowExplorerPaneProps = Record<string, never>;

export function WorkflowExplorerPane() {
  const workflows = useSelector<InspectorState, WorkflowSummary[]>((s) => s.workflows);
  const loading = useSelector<InspectorState, boolean>((s) => s.workflowsLoading);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const [search, setSearch] = useState("");
  const searchTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Request workflows on mount
  useEffect(() => {
    dispatch({ type: "ui/workflows_requested", payload: {} });
  }, [dispatch]);

  const handleSearchChange = useCallback(
    (value: string) => {
      setSearch(value);
      if (searchTimerRef.current) clearTimeout(searchTimerRef.current);
      searchTimerRef.current = setTimeout(() => {
        dispatch({ type: "ui/workflows_requested", payload: { search: value || undefined } });
      }, 300);
    },
    [dispatch],
  );

  const handleRowClick = useCallback(
    (workflowId: string) => {
      dispatch({ type: "ui/flow_opened", payload: { workflowId } });
    },
    [dispatch],
  );

  const handleCopy = useCallback((text: string) => {
    navigator.clipboard.writeText(text).catch(() => {});
  }, []);

  return (
    <div className="flex flex-col h-full">
      {/* Search bar */}
      <div className="px-3 py-2.5 border-b border-border" style={{ background: "rgba(15, 15, 20, 0.6)", backdropFilter: "blur(8px)" }}>
        <input
          type="text"
          placeholder="Search by workflow ID or event type..."
          value={search}
          onChange={(e) => handleSearchChange(e.target.value)}
          className="w-full px-3 py-1.5 text-xs bg-background/50 border border-border rounded-md text-foreground placeholder:text-muted-foreground/40 focus:outline-none focus:ring-1 focus:ring-indigo-500/40 focus:border-indigo-500/30 transition-all"
        />
      </div>

      {/* Table header */}
      <div className="flex items-center gap-2 px-3 py-2 border-b border-border text-[9px] font-semibold text-muted-foreground/40 uppercase tracking-widest">
        <span className="w-28 shrink-0">Root Event</span>
        <span className="w-24 shrink-0">Workflow</span>
        <span className="w-12 shrink-0 text-right">Events</span>
        <span className="w-20 shrink-0 text-right">Duration</span>
        <span className="flex-1">Last Activity</span>
      </div>

      {/* Content */}
      {loading && workflows.length === 0 ? (
        <div className="animate-pulse p-3">
          {Array.from({ length: 8 }).map((_, i) => (
            <div key={i} className="flex items-center gap-2 py-2.5">
              <div className="h-3 w-28 bg-white/[0.03] rounded" />
              <div className="h-3 w-24 bg-white/[0.03] rounded" />
              <div className="h-3 w-12 bg-white/[0.03] rounded" />
            </div>
          ))}
        </div>
      ) : workflows.length === 0 ? (
        <div className="flex items-center justify-center h-32 text-xs text-muted-foreground/50 tracking-wide">
          No workflows found
        </div>
      ) : (
        <div className="flex-1 overflow-y-auto">
          {workflows.map((corr) => (
            <button
              key={corr.workflowId}
              onClick={() => handleRowClick(corr.workflowId)}
              className="group w-full text-left flex items-center gap-2 px-3 py-2.5 border-b border-border hover:bg-indigo-500/8 transition-all duration-150"
            >
              {/* Root event type badge */}
              <span
                className="text-[10px] font-mono shrink-0 w-28 truncate px-1.5 py-0.5 rounded"
                style={{
                  color: eventTextColor(corr.rootEventType),
                  background: eventBg(corr.rootEventType),
                }}
                title={corr.rootEventType}
              >
                {corr.rootEventType}
              </span>

              {/* Workflow ID */}
              <span
                className="text-[10px] font-mono text-purple-400/70 w-24 shrink-0 truncate cursor-pointer hover:text-purple-400 transition-colors"
                title={`Click to copy: ${corr.workflowId}`}
                onClick={(e) => { e.stopPropagation(); handleCopy(corr.workflowId); }}
              >
                {corr.workflowId.slice(0, 8)}
              </span>

              {/* Event count */}
              <span className="text-[11px] font-mono text-foreground/70 w-12 shrink-0 text-right tabular-nums">
                {corr.eventCount}
              </span>

              {/* Duration */}
              <span className="text-[10px] text-muted-foreground/50 w-20 shrink-0 text-right font-mono tabular-nums">
                <RelativeDuration firstTs={corr.firstTs} lastTs={corr.lastTs} />
              </span>

              {/* Last activity */}
              <span className="text-[10px] text-muted-foreground/40 flex-1 truncate tabular-nums">
                {formatTs(corr.lastTs)}
              </span>

              {/* Error indicator */}
              {corr.hasErrors && (
                <span
                  className="flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] font-semibold shrink-0 bg-red-500/10 text-red-400/80 border border-red-500/20"
                  style={{ boxShadow: "0 0 6px rgba(239, 68, 68, 0.15)" }}
                  title="This workflow has errors"
                >
                  error
                </span>
              )}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
