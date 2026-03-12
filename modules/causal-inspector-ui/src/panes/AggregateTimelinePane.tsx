import { useMemo, useCallback, useEffect, useRef, useState } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { AggregateTimelineEntry } from "../types";
import { eventBg, eventBorder, eventTextColor } from "../theme";

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
// Collapsible aggregate card
// ---------------------------------------------------------------------------

function AggregateCard({
  aggregateKey,
  state,
}: {
  aggregateKey: string;
  state: unknown;
}) {
  const [collapsed, setCollapsed] = useState(false);

  // Parse "AggType:id" into type + id for display
  const colonIdx = aggregateKey.indexOf(":");
  const aggType = colonIdx > 0 ? aggregateKey.slice(0, colonIdx) : aggregateKey;
  const aggId = colonIdx > 0 ? aggregateKey.slice(colonIdx + 1) : null;

  return (
    <div
      style={{
        background: "#18181b",
        border: "1px solid #27272a",
        borderRadius: 6,
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
          borderBottom: collapsed ? "none" : "1px solid #27272a",
        }}
      >
        <span style={{ fontSize: 9, color: "#52525b", transform: collapsed ? "rotate(-90deg)" : "rotate(0deg)", transition: "transform 100ms" }}>
          &#9660;
        </span>
        <span style={{ fontSize: 11, fontWeight: 500, color: "#e4e4e7" }}>{aggType}</span>
        {aggId && (
          <span style={{ fontSize: 10, color: "#71717a", fontFamily: "monospace" }}>{aggId}</span>
        )}
      </div>
      {!collapsed && (
        <div style={{ padding: "4px 8px 6px" }}>
          <InlineJson value={state} />
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
  const scrubberPosition = useSelector<InspectorState, number | null>((s) => s.scrubberPosition);
  const dispatch = useDispatch<InspectorMachineEvent>();

  // Auto-scroll to current row when scrubber moves
  const currentRowRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (scrubberPosition != null && currentRowRef.current) {
      currentRowRef.current.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  }, [scrubberPosition]);

  const handleRowClick = useCallback(
    (seq: number) => {
      dispatch({ type: "ui/scrubber_moved", payload: { position: seq } });
    },
    [dispatch],
  );

  if (!correlationId) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#71717a", fontSize: 13 }}>
        Open a flow to see the aggregate state timeline
      </div>
    );
  }

  if (entries.length === 0) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#71717a", fontSize: 13 }}>
        No aggregate state snapshots for this correlation
      </div>
    );
  }

  return (
    <div style={{ height: "100%", overflow: "auto", padding: "8px 12px" }}>
      {entries.map((entry) => {
        const isFuture = scrubberPosition != null && entry.seq > scrubberPosition;
        const isCurrent = scrubberPosition === entry.seq;

        return (
          <div
            key={entry.seq}
            ref={isCurrent ? currentRowRef : undefined}
            onClick={() => handleRowClick(entry.seq)}
            style={{
              opacity: isFuture ? 0.3 : 1,
              borderLeft: isCurrent ? "3px solid #3b82f6" : "3px solid transparent",
              padding: "6px 8px",
              marginBottom: 4,
              borderRadius: 4,
              background: isCurrent ? "rgba(59,130,246,0.1)" : "transparent",
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
                  border: `1px solid ${eventBorder(entry.eventType)}`,
                  borderRadius: 4,
                  padding: "1px 6px",
                }}
              >
                {entry.eventType}
              </div>
            </div>

            {/* Aggregate state cards */}
            <div style={{ display: "flex", flexWrap: "wrap", gap: 8, marginLeft: 46 }}>
              {entry.aggregates.map((agg) => (
                <AggregateCard key={agg.key} aggregateKey={agg.key} state={agg.state} />
              ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}
