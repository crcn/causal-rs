import { useMemo, useCallback, useEffect, useRef, useState } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { AggregateTimelineEntry, InspectorEvent } from "../types";
import { eventBg, eventBorder, eventTextColor } from "../theme";
import { inScrubberRange } from "../utils";
import { X } from "lucide-react";

// ---------------------------------------------------------------------------
// Inline JSON syntax highlighting (inline styles — no Tailwind dependency)
// ---------------------------------------------------------------------------

type JsonToken = { text: string; color: string };

function tokenizeJson(json: string): JsonToken[] {
  const tokens: JsonToken[] = [];
  const re =
    /("(?:[^"\\]|\\.)*")\s*:|("(?:[^"\\]|\\.)*")|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|(\btrue\b|\bfalse\b)|(\bnull\b)|([{}[\]:,])/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;

  while ((match = re.exec(json)) !== null) {
    if (match.index > lastIndex) {
      tokens.push({ text: json.slice(lastIndex, match.index), color: "" });
    }
    if (match[1] !== undefined) {
      tokens.push({ text: match[1], color: "#60a5fa" }); // key — blue
      tokens.push({ text: ":", color: "#71717a" });
    } else if (match[2] !== undefined) {
      tokens.push({ text: match[2], color: "#4ade80" }); // string — green
    } else if (match[3] !== undefined) {
      tokens.push({ text: match[3], color: "#fbbf24" }); // number — amber
    } else if (match[4] !== undefined) {
      tokens.push({ text: match[4], color: "#c084fc" }); // bool — purple
    } else if (match[5] !== undefined) {
      tokens.push({ text: match[5], color: "#71717a" }); // null — gray
    } else if (match[6] !== undefined) {
      tokens.push({ text: match[6], color: "#71717a" }); // punctuation
    }
    lastIndex = re.lastIndex;
  }
  if (lastIndex < json.length) {
    tokens.push({ text: json.slice(lastIndex), color: "" });
  }
  return tokens;
}

function InlineJson({ value }: { value: unknown }) {
  const json = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  const tokens = useMemo(() => tokenizeJson(json), [json]);
  return (
    <pre
      style={{
        margin: 0,
        fontSize: 11,
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
        whiteSpace: "pre-wrap",
        wordBreak: "break-word",
        lineHeight: 1.5,
      }}
    >
      {tokens.map((t, i) => (
        <span key={i} style={{ color: t.color || undefined }}>
          {t.text}
        </span>
      ))}
    </pre>
  );
}

// ---------------------------------------------------------------------------
// JSON Diff
// ---------------------------------------------------------------------------

type DiffEntry =
  | { kind: "added"; key: string; value: unknown }
  | { kind: "removed"; key: string; value: unknown }
  | { kind: "changed"; key: string; oldValue: unknown; newValue: unknown };

function jsonDiff(prev: unknown, curr: unknown): DiffEntry[] {
  const diffs: DiffEntry[] = [];

  if (prev == null && curr == null) return diffs;
  if (prev == null) {
    // Everything is added
    if (typeof curr === "object" && curr !== null && !Array.isArray(curr)) {
      for (const key of Object.keys(curr as Record<string, unknown>)) {
        diffs.push({ kind: "added", key, value: (curr as Record<string, unknown>)[key] });
      }
    }
    return diffs;
  }
  if (curr == null) {
    if (typeof prev === "object" && prev !== null && !Array.isArray(prev)) {
      for (const key of Object.keys(prev as Record<string, unknown>)) {
        diffs.push({ kind: "removed", key, value: (prev as Record<string, unknown>)[key] });
      }
    }
    return diffs;
  }

  const prevObj = (typeof prev === "object" && !Array.isArray(prev) ? prev : {}) as Record<string, unknown>;
  const currObj = (typeof curr === "object" && !Array.isArray(curr) ? curr : {}) as Record<string, unknown>;

  const allKeys = new Set([...Object.keys(prevObj), ...Object.keys(currObj)]);

  for (const key of allKeys) {
    const inPrev = key in prevObj;
    const inCurr = key in currObj;

    if (inCurr && !inPrev) {
      diffs.push({ kind: "added", key, value: currObj[key] });
    } else if (inPrev && !inCurr) {
      diffs.push({ kind: "removed", key, value: prevObj[key] });
    } else if (JSON.stringify(prevObj[key]) !== JSON.stringify(currObj[key])) {
      diffs.push({ kind: "changed", key, oldValue: prevObj[key], newValue: currObj[key] });
    }
  }

  return diffs;
}

function DiffView({ diffs }: { diffs: DiffEntry[] }) {
  if (diffs.length === 0) {
    return (
      <div style={{ fontSize: 11, color: "#52525b", fontStyle: "italic", padding: "2px 0" }}>
        No changes
      </div>
    );
  }

  const mono: React.CSSProperties = {
    fontSize: 11,
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    lineHeight: 1.5,
    wordBreak: "break-word",
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
      {diffs.map((d) => {
        if (d.kind === "added") {
          return (
            <div key={d.key} style={{ ...mono, color: "#4ade80", background: "rgba(74,222,128,0.08)", borderRadius: 3, padding: "1px 4px" }}>
              <span style={{ color: "#22c55e", fontWeight: 600 }}>+ </span>
              <span style={{ color: "#60a5fa" }}>{d.key}</span>
              <span style={{ color: "#71717a" }}>: </span>
              <span>{JSON.stringify(d.value)}</span>
            </div>
          );
        }
        if (d.kind === "removed") {
          return (
            <div key={d.key} style={{ ...mono, color: "#f87171", background: "rgba(248,113,113,0.08)", borderRadius: 3, padding: "1px 4px" }}>
              <span style={{ color: "#ef4444", fontWeight: 600 }}>- </span>
              <span style={{ color: "#60a5fa" }}>{d.key}</span>
              <span style={{ color: "#71717a" }}>: </span>
              <span>{JSON.stringify(d.value)}</span>
            </div>
          );
        }
        // changed
        return (
          <div key={d.key} style={{ ...mono, background: "rgba(251,191,36,0.08)", borderRadius: 3, padding: "1px 4px" }}>
            <span style={{ color: "#fbbf24", fontWeight: 600 }}>~ </span>
            <span style={{ color: "#60a5fa" }}>{d.key}</span>
            <span style={{ color: "#71717a" }}>: </span>
            <span style={{ color: "#f87171", textDecoration: "line-through" }}>{JSON.stringify(d.oldValue)}</span>
            <span style={{ color: "#71717a" }}> → </span>
            <span style={{ color: "#4ade80" }}>{JSON.stringify(d.newValue)}</span>
          </div>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Collapsible aggregate card
// ---------------------------------------------------------------------------

function AggregateCard({
  aggregateKey,
  state,
  prevState,
  diffMode,
}: {
  aggregateKey: string;
  state: unknown;
  prevState: unknown | undefined;
  diffMode: boolean;
}) {
  const [collapsed, setCollapsed] = useState(false);

  // Parse "AggType:id" into type + id for display
  const colonIdx = aggregateKey.indexOf(":");
  const aggType = colonIdx > 0 ? aggregateKey.slice(0, colonIdx) : aggregateKey;
  const aggId = colonIdx > 0 ? aggregateKey.slice(colonIdx + 1) : null;

  const diffs = useMemo(() => {
    if (!diffMode || prevState === undefined) return [];
    return jsonDiff(prevState, state);
  }, [diffMode, prevState, state]);

  const showDiff = diffMode && prevState !== undefined;

  return (
    <div
      style={{
        background: "rgba(255, 255, 255, 0.02)",
        border: "1px solid rgba(255, 255, 255, 0.06)",
        borderRadius: 8,
        overflow: "hidden",
        flex: "1 1 280px",
        maxWidth: 500,
      }}
    >
      <div
        onClick={() => setCollapsed(!collapsed)}
        style={{
          display: "flex",
          alignItems: "center",
          gap: 6,
          padding: "4px 8px",
          cursor: "pointer",
          userSelect: "none",
          borderBottom: collapsed ? "none" : "1px solid rgba(255,255,255,0.06)",
        }}
      >
        <span style={{ fontSize: 9, color: "#52525b", transform: collapsed ? "rotate(-90deg)" : "rotate(0deg)", transition: "transform 100ms" }}>
          &#9660;
        </span>
        <span style={{ fontSize: 11, fontWeight: 500, color: "#e4e4e7" }}>{aggType}</span>
        {aggId && (
          <span style={{ fontSize: 10, color: "#71717a", fontFamily: "monospace" }}>{aggId}</span>
        )}
        {showDiff && diffs.length > 0 && (
          <span style={{ fontSize: 9, color: "#fbbf24", marginLeft: "auto" }}>
            {diffs.length} change{diffs.length !== 1 ? "s" : ""}
          </span>
        )}
        {showDiff && diffs.length === 0 && (
          <span style={{ fontSize: 9, color: "#52525b", marginLeft: "auto" }}>unchanged</span>
        )}
      </div>
      {!collapsed && (
        <div style={{ padding: "4px 8px 6px" }}>
          {showDiff ? <DiffView diffs={diffs} /> : <InlineJson value={state} />}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// AggregateTimelinePane
// ---------------------------------------------------------------------------

export type AggregateTimelinePaneProps = Record<string, never>;

export function AggregateTimelinePane() {
  const correlationId = useSelector<InspectorState, string | null>((s) => s.flowCorrelationId);
  const entries = useSelector<InspectorState, AggregateTimelineEntry[]>((s) =>
    correlationId ? s.aggregateTimeline[correlationId] ?? [] : [],
  );
  const scrubberStart = useSelector<InspectorState, number | null>((s) => s.scrubberStart);
  const scrubberEnd = useSelector<InspectorState, number | null>((s) => s.scrubberEnd);
  const logsFilter = useSelector<InspectorState, { reactorId: string | null }>((s) => s.logsFilter);
  const flowData = useSelector<InspectorState, InspectorEvent[]>((s) => s.flowData);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const [diffMode, setDiffMode] = useState(false);

  // Map eventId → reactorId for handler selection highlighting
  const eventReactorMap = useMemo(() => {
    const map = new Map<string, string>();
    for (const e of flowData) {
      if (e.id && e.reactorId) map.set(e.id, e.reactorId);
    }
    return map;
  }, [flowData]);

  const hasReactorFilter = logsFilter.reactorId != null;

  // Build a lookup: seq → { aggKey → state } for computing diffs
  const prevStateMap = useMemo(() => {
    const map = new Map<number, Map<string, unknown>>();
    for (let i = 1; i < entries.length; i++) {
      const prev = entries[i - 1];
      const prevAggs = new Map<string, unknown>();
      for (const agg of prev.aggregates) {
        prevAggs.set(agg.key, agg.state);
      }
      map.set(entries[i].seq, prevAggs);
    }
    return map;
  }, [entries]);

  // Auto-scroll to current row when scrubber moves
  const currentRowRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (scrubberEnd != null && currentRowRef.current) {
      currentRowRef.current.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  }, [scrubberEnd]);

  const handleRowClick = useCallback(
    (seq: number) => {
      dispatch({ type: "ui/scrubber_end_changed", payload: { end: seq } });
    },
    [dispatch],
  );

  if (!correlationId) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#50506a", fontSize: 12, letterSpacing: "0.03em" }}>
        Open a flow to see the aggregate state timeline
      </div>
    );
  }

  if (entries.length === 0) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#50506a", fontSize: 12, letterSpacing: "0.03em" }}>
        No aggregate state snapshots for this correlation
      </div>
    );
  }

  return (
    <div style={{ height: "100%", display: "flex", flexDirection: "column" }}>
      {/* Toolbar */}
      <div style={{ display: "flex", alignItems: "center", gap: 4, padding: "6px 12px", borderBottom: "1px solid rgba(255,255,255,0.06)", flexShrink: 0, background: "rgba(15, 15, 20, 0.6)", backdropFilter: "blur(8px)" }}>
        <button
          onClick={() => setDiffMode(false)}
          style={{
            fontSize: 10,
            padding: "3px 10px",
            borderRadius: 6,
            border: "1px solid",
            borderColor: !diffMode ? "rgba(99,102,241,0.3)" : "rgba(255,255,255,0.06)",
            background: !diffMode ? "rgba(99,102,241,0.12)" : "transparent",
            color: !diffMode ? "#818cf8" : "#70708a",
            cursor: "pointer",
            transition: "all 150ms",
            letterSpacing: "0.02em",
          }}
        >
          Full State
        </button>
        <button
          onClick={() => setDiffMode(true)}
          style={{
            fontSize: 10,
            padding: "3px 10px",
            borderRadius: 6,
            border: "1px solid",
            borderColor: diffMode ? "rgba(99,102,241,0.3)" : "rgba(255,255,255,0.06)",
            background: diffMode ? "rgba(99,102,241,0.12)" : "transparent",
            color: diffMode ? "#818cf8" : "#70708a",
            cursor: "pointer",
            transition: "all 150ms",
            letterSpacing: "0.02em",
          }}
        >
          Diff
        </button>
        {hasReactorFilter && (
          <>
            <div style={{ width: 1, height: 14, background: "rgba(255,255,255,0.06)", marginLeft: 4, marginRight: 4 }} />
            <span style={{ fontSize: 10, color: "#818cf8", fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace" }}>
              {logsFilter.reactorId}
            </span>
            <button
              onClick={() => dispatch({ type: "ui/flow_node_selected", payload: null })}
              style={{ background: "none", border: "none", cursor: "pointer", color: "#70708a", padding: 2, borderRadius: 4, display: "flex", alignItems: "center" }}
              title="Clear selection"
            >
              <X size={12} />
            </button>
          </>
        )}
      </div>

      {/* Timeline entries */}
      <div style={{ flex: 1, overflow: "auto", padding: "8px 12px" }}>
        {entries.map((entry, entryIdx) => {
          const isOutside = (scrubberStart != null || scrubberEnd != null) && !inScrubberRange(entry.seq, scrubberStart, scrubberEnd);
          const isCurrent = scrubberEnd === entry.seq;
          const prevAggs = prevStateMap.get(entry.seq);
          const entryReactorId = entry.eventId ? eventReactorMap.get(entry.eventId) : null;
          const isReactorMatch = !hasReactorFilter || entryReactorId === logsFilter.reactorId;

          return (
            <div
              key={entry.seq}
              ref={isCurrent ? currentRowRef : undefined}
              onClick={() => handleRowClick(entry.seq)}
              style={{
                opacity: isOutside ? 0.3 : !isReactorMatch ? 0.35 : 1,
                padding: "6px 8px",
                marginBottom: 4,
                borderRadius: 6,
                background: isCurrent ? "rgba(99,102,241,0.15)" : isReactorMatch && hasReactorFilter ? "rgba(99,102,241,0.06)" : "transparent",
                cursor: "pointer",
                transition: "opacity 150ms, background 150ms",
              }}
            >
              {/* Event header */}
              <div style={{ display: "flex", alignItems: "center", gap: 6, marginBottom: 4 }}>
                <div style={{ fontSize: 10, fontWeight: 600, color: "#a1a1aa", minWidth: 40 }}>
                  #{entry.seq}
                </div>
                <div
                  style={{
                    fontSize: 11,
                    fontWeight: 500,
                    color: eventTextColor(entry.eventType),
                    background: eventBg(entry.eventType),
                    borderRadius: 4,
                    padding: "1px 6px",
                  }}
                >
                  {entry.eventType}
                </div>
                {diffMode && entryIdx === 0 && (
                  <span style={{ fontSize: 9, color: "#52525b", fontStyle: "italic" }}>initial state</span>
                )}
              </div>

              {/* Aggregate state cards */}
              <div style={{ display: "flex", flexWrap: "wrap", gap: 8, marginLeft: 46 }}>
                {entry.aggregates.map((agg) => (
                  <AggregateCard
                    key={agg.key}
                    aggregateKey={agg.key}
                    state={agg.state}
                    prevState={prevAggs?.get(agg.key)}
                    diffMode={diffMode && entryIdx > 0}
                  />
                ))}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
