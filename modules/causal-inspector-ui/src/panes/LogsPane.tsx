import { useState, useMemo } from "react";
import { useSelector } from "../machine";
import type { InspectorState } from "../state";
import type { ReactorLog, LogsFilter } from "../types";
import { LOG_LEVEL_COLORS } from "../theme";
import { formatTs, inScrubberRange } from "../utils";
import { Search, ChevronRight, ChevronDown } from "lucide-react";

// ---------------------------------------------------------------------------
// LogRow
// ---------------------------------------------------------------------------

function LogRow({ log, showReactor }: { log: ReactorLog; showReactor: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const levelColor = LOG_LEVEL_COLORS[log.level] ?? "bg-zinc-600/20 text-zinc-400";

  return (
    <div className="px-2 py-1.5 hover:bg-white/[0.02] rounded-md transition-colors duration-100">
      <div className="flex items-center gap-2 min-w-0">
        <span className={`px-1.5 py-0.5 rounded text-[9px] font-semibold uppercase shrink-0 ${levelColor}`}>
          {log.level}
        </span>
        <span className="text-[10px] text-muted-foreground/50 shrink-0 tabular-nums">
          {formatTs(log.loggedAt)}
        </span>
        {showReactor && (
          <span className="text-[10px] font-mono text-muted-foreground/40 shrink-0">
            {log.reactorId}
          </span>
        )}
        <span className="text-[11px] text-foreground/80 truncate">{log.message}</span>
        {log.data != null && (
          <button
            onClick={() => setExpanded((v) => !v)}
            className="ml-auto text-[10px] p-1 rounded-md hover:bg-white/[0.05] shrink-0 text-muted-foreground/50 hover:text-foreground transition-colors"
          >
            {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
          </button>
        )}
      </div>
      {expanded && log.data != null && (
        <pre className="mt-1.5 ml-4 text-[10px] font-mono text-muted-foreground/70 bg-white/[0.02] rounded-md p-2.5 max-h-32 overflow-auto whitespace-pre-wrap border border-border">
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
  const scrubberStart = useSelector<InspectorState, number | null>((s) => s.scrubberStart);
  const scrubberEnd = useSelector<InspectorState, number | null>((s) => s.scrubberEnd);

  const [levelFilter, setLevelFilter] = useState<Set<string>>(new Set(["debug", "info", "warn", "error"]));
  const [searchText, setSearchText] = useState("");

  const isCorrelationScope = logsFilter.scope === "correlation" && logsFilter.correlationId != null;
  const hasFilter = logsFilter.reactorId != null || isCorrelationScope;

  // Set of event IDs visible within scrubber range
  const visibleEventIds = useMemo(() => {
    if (scrubberStart == null && scrubberEnd == null) return null;
    return new Set(
      flowData.filter((e) => inScrubberRange(e.seq, scrubberStart, scrubberEnd)).map((e) => e.id).filter(Boolean),
    );
  }, [flowData, scrubberStart, scrubberEnd]);

  // Client-side filtering
  const filteredLogs = useMemo(() => {
    let filtered = logs.filter((l) => levelFilter.has(l.level));
    // Filter by reactor when handler is selected
    if (logsFilter.scope === "reactor" && logsFilter.reactorId) {
      filtered = filtered.filter((l) => l.reactorId === logsFilter.reactorId);
    }
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
  }, [logs, logsFilter, levelFilter, searchText, visibleEventIds]);

  if (!hasFilter) {
    return (
      <div className="flex items-center justify-center h-full text-xs text-muted-foreground/50 tracking-wide">
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
      <div className="px-3 py-2 border-b border-border flex items-center gap-3 flex-wrap" style={{ background: "rgba(15, 15, 20, 0.6)", backdropFilter: "blur(8px)" }}>
        {/* Level filters */}
        <div className="flex items-center gap-1 text-[10px]">
          {["debug", "info", "warn", "error"].map((level) => (
            <button
              key={level}
              onClick={() => toggleLevel(level)}
              className={`px-2 py-0.5 rounded-md uppercase font-semibold transition-all duration-150 ${
                levelFilter.has(level)
                  ? LOG_LEVEL_COLORS[level]
                  : "text-muted-foreground/30 line-through"
              }`}
            >
              {level}
            </button>
          ))}
        </div>

        {/* Search */}
        <div className="relative flex items-center">
          <Search size={10} className="absolute left-2.5 text-muted-foreground/40 pointer-events-none" />
          <input
            type="text"
            placeholder="Search logs..."
            value={searchText}
            onChange={(e) => setSearchText(e.target.value)}
            className="pl-7 pr-2 text-[11px] bg-background/50 border border-border rounded-md py-1 w-44 focus:outline-none focus:ring-1 focus:ring-indigo-500/40 focus:border-indigo-500/30 text-foreground placeholder:text-muted-foreground/40 transition-all"
          />
        </div>

        {/* Investigate */}
        {onInvestigate && (
          <button
            onClick={() => onInvestigate(logsFilter)}
            className="flex items-center gap-1 px-2 py-1 rounded-md text-[11px] text-muted-foreground/50 hover:text-foreground hover:bg-white/[0.04] transition-all duration-150"
            title="Investigate logs"
          >
            <Search size={12} />
          </button>
        )}
      </div>

      {/* Header */}
      <div className="px-3 py-1.5 text-[10px] text-muted-foreground/50">
        {logsFilter.reactorId ? (
          <>
            <span className="font-mono text-foreground/60">{logsFilter.reactorId}</span>
            {isCorrelationScope && <span className="ml-1">(all reactors in correlation)</span>}
          </>
        ) : (
          <span>All reactors in correlation</span>
        )}
        <span className="ml-2 tabular-nums">{filteredLogs.length} logs</span>
      </div>

      {/* Log list */}
      <div className="flex-1 overflow-y-auto px-1">
        {filteredLogs.length === 0 && (
          <div className="p-3 text-[11px] text-muted-foreground/40">No logs match filters</div>
        )}
        {filteredLogs.map((log, i) => (
          <LogRow key={i} log={log} showReactor={isCorrelationScope} />
        ))}
      </div>
    </div>
  );
}
