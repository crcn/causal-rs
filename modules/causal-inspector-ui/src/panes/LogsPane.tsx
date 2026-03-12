import { useState, useMemo } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { ReactorLog, LogsFilter } from "../types";
import { LOG_LEVEL_COLORS } from "../theme";
import { formatTs } from "../utils";
import { Search, ChevronRight, ChevronDown } from "lucide-react";

// ---------------------------------------------------------------------------
// LogRow
// ---------------------------------------------------------------------------

function LogRow({ log, showReactor }: { log: ReactorLog; showReactor: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const levelColor = LOG_LEVEL_COLORS[log.level] ?? "bg-zinc-600/30 text-zinc-400";

  return (
    <div className="px-2 py-1 hover:bg-accent/20 rounded">
      <div className="flex items-center gap-1.5 min-w-0">
        <span className={`px-1 py-0.5 rounded text-[10px] font-semibold uppercase shrink-0 ${levelColor}`}>
          {log.level}
        </span>
        <span className="text-[10px] text-muted-foreground shrink-0">
          {formatTs(log.loggedAt)}
        </span>
        {showReactor && (
          <span className="text-[10px] font-mono text-zinc-500 shrink-0">
            {log.reactorId}
          </span>
        )}
        <span className="text-[11px] text-zinc-200 truncate">{log.message}</span>
        {log.data != null && (
          <button
            onClick={() => setExpanded((v) => !v)}
            className="ml-auto text-[10px] px-1 py-0.5 rounded hover:bg-accent shrink-0 text-muted-foreground hover:text-foreground"
          >
            {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
          </button>
        )}
      </div>
      {expanded && log.data != null && (
        <pre className="mt-1 ml-4 text-[10px] font-mono text-zinc-400 bg-zinc-900/50 rounded p-2 max-h-32 overflow-auto whitespace-pre-wrap">
          {typeof log.data === "string" ? log.data : JSON.stringify(log.data, null, 2)}
        </pre>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// LogsPane
// ---------------------------------------------------------------------------

export type LogsPaneProps = {
  /** Optional callback when user wants to investigate logs. */
  onInvestigate?: (filter: LogsFilter) => void;
};

export function LogsPane({ onInvestigate }: LogsPaneProps = {}) {
  const logs = useSelector<InspectorState, ReactorLog[]>((s) => s.logs);
  const logsFilter = useSelector<InspectorState, LogsFilter>((s) => s.logsFilter);
  const flowData = useSelector<InspectorState, InspectorState["flowData"]>((s) => s.flowData);
  const scrubberPosition = useSelector<InspectorState, number | null>((s) => s.scrubberPosition);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const [levelFilter, setLevelFilter] = useState<Set<string>>(new Set(["debug", "info", "warn"]));
  const [searchText, setSearchText] = useState("");

  const isCorrelationScope = logsFilter.scope === "correlation" && logsFilter.correlationId != null;
  const hasFilter = logsFilter.eventId != null || logsFilter.reactorId != null || isCorrelationScope;

  // Set of event IDs visible at current scrubber position
  const visibleEventIds = useMemo(() => {
    if (scrubberPosition == null) return null;
    return new Set(
      flowData.filter((e) => e.seq <= scrubberPosition).map((e) => e.id).filter(Boolean),
    );
  }, [flowData, scrubberPosition]);

  // Client-side filtering
  const filteredLogs = useMemo(() => {
    let filtered = logs.filter((l) => levelFilter.has(l.level));
    if (visibleEventIds != null) {
      filtered = filtered.filter((l) => visibleEventIds.has(l.eventId));
    }
    if (searchText) {
      const lower = searchText.toLowerCase();
      filtered = filtered.filter(
        (l) =>
          l.message.toLowerCase().includes(lower) ||
          l.reactorId.toLowerCase().includes(lower) ||
          (l.data && String(l.data).toLowerCase().includes(lower)),
      );
    }
    return filtered;
  }, [logs, levelFilter, searchText, visibleEventIds]);

  if (!hasFilter) {
    return (
      <div className="flex items-center justify-center h-full text-sm text-muted-foreground">
        Click a reactor node in the causal tree to view logs
      </div>
    );
  }

  const toggleLevel = (level: string) => {
    setLevelFilter((prev) => {
      const next = new Set(prev);
      if (next.has(level)) next.delete(level);
      else next.add(level);
      return next;
    });
  };

  return (
    <div className="h-full flex flex-col">
      {/* Toolbar */}
      <div className="px-3 py-2 border-b border-border flex items-center gap-3 flex-wrap">
        {/* Scope toggle */}
        <div className="flex items-center gap-1 text-[11px]">
          <button
            onClick={() => dispatch({ type: "ui/logs_filter_changed", payload: { scope: "reactor" } })}
            className={`px-2 py-0.5 rounded ${logsFilter.scope === "reactor" ? "bg-accent text-foreground" : "text-muted-foreground hover:text-foreground"}`}
          >
            This reactor
          </button>
          {logsFilter.correlationId && (
            <button
              onClick={() => dispatch({ type: "ui/logs_filter_changed", payload: { scope: "correlation" } })}
              className={`px-2 py-0.5 rounded ${logsFilter.scope === "correlation" ? "bg-accent text-foreground" : "text-muted-foreground hover:text-foreground"}`}
            >
              This correlation
            </button>
          )}
        </div>

        {/* Level filters */}
        <div className="flex items-center gap-1 text-[10px]">
          {["debug", "info", "warn"].map((level) => (
            <button
              key={level}
              onClick={() => toggleLevel(level)}
              className={`px-1.5 py-0.5 rounded uppercase font-semibold ${
                levelFilter.has(level)
                  ? LOG_LEVEL_COLORS[level]
                  : "text-zinc-600 line-through"
              }`}
            >
              {level}
            </button>
          ))}
        </div>

        {/* Search */}
        <div className="relative flex items-center ml-auto">
          <Search size={10} className="absolute left-2 text-muted-foreground pointer-events-none" />
          <input
            type="text"
            placeholder="Search logs..."
            value={searchText}
            onChange={(e) => setSearchText(e.target.value)}
            className="pl-6 pr-2 text-[11px] bg-transparent border border-border rounded py-0.5 w-40 focus:outline-none focus:ring-1 focus:ring-blue-500/50 text-foreground placeholder:text-muted-foreground"
          />
        </div>

        {/* Investigate */}
        {onInvestigate && (
          <button
            onClick={() => onInvestigate(logsFilter)}
            className="flex items-center gap-1 px-2 py-0.5 rounded text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition-colors"
            title="Investigate logs"
          >
            <Search size={12} />
          </button>
        )}
      </div>

      {/* Header */}
      <div className="px-3 py-1.5 text-[10px] text-muted-foreground">
        {logsFilter.reactorId ? (
          <>
            <span className="font-mono">{logsFilter.reactorId}</span>
            {isCorrelationScope && <span className="ml-1">(all reactors in correlation)</span>}
          </>
        ) : (
          <span>All reactors in correlation</span>
        )}
        <span className="ml-2">{filteredLogs.length} logs</span>
      </div>

      {/* Log list */}
      <div className="flex-1 overflow-y-auto px-1">
        {filteredLogs.length === 0 && (
          <div className="p-3 text-[11px] text-muted-foreground">No logs match filters</div>
        )}
        {filteredLogs.map((log, i) => (
          <LogRow key={i} log={log} showReactor={isCorrelationScope} />
        ))}
      </div>
    </div>
  );
}
