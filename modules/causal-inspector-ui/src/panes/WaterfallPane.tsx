import { useMemo } from "react";
import { useSelector } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorEvent, ReactorOutcome } from "../types";

// ---------------------------------------------------------------------------
// Timing helpers
// ---------------------------------------------------------------------------

function parseTime(ts: string | null): number | null {
  if (!ts) return null;
  return new Date(ts).getTime();
}

function formatDuration(ms: number): string {
  if (ms < 1) return "<1ms";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${(ms / 60_000).toFixed(1)}m`;
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

const STATUS_COLORS: Record<string, { bar: string; text: string }> = {
  completed: { bar: "#22c55e", text: "#bbf7d0" },
  running: { bar: "#eab308", text: "#fef08a" },
  error: { bar: "#ef4444", text: "#fecaca" },
};

function statusColor(status: string) {
  return STATUS_COLORS[status] ?? STATUS_COLORS.running;
}

// ---------------------------------------------------------------------------
// Bar data
// ---------------------------------------------------------------------------

type WaterfallBar = {
  reactorId: string;
  status: string;
  error: string | null;
  attempts: number;
  startMs: number;
  endMs: number;
  triggeringEventIds: string[];
};

function buildBars(outcomes: ReactorOutcome[]): {
  bars: WaterfallBar[];
  minMs: number;
  maxMs: number;
} {
  const bars: WaterfallBar[] = [];
  let minMs = Infinity;
  let maxMs = -Infinity;

  for (const o of outcomes) {
    const start = parseTime(o.startedAt);
    if (start == null) continue; // can't render without start

    const end = parseTime(o.completedAt) ?? Date.now();
    minMs = Math.min(minMs, start);
    maxMs = Math.max(maxMs, end);

    bars.push({
      reactorId: o.reactorId,
      status: o.status,
      error: o.error,
      attempts: o.attempts,
      startMs: start,
      endMs: end,
      triggeringEventIds: o.triggeringEventIds,
    });
  }

  // Sort by start time
  bars.sort((a, b) => a.startMs - b.startMs);

  return { bars, minMs, maxMs };
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ROW_HEIGHT = 32;
const LABEL_WIDTH = 160;
const BAR_MIN_WIDTH = 4;
const PADDING_X = 12;

// ---------------------------------------------------------------------------
// WaterfallPane
// ---------------------------------------------------------------------------

export type WaterfallPaneProps = Record<string, never>;

export function WaterfallPane() {
  const correlationId = useSelector<InspectorState, string | null>((s) => s.flowCorrelationId);
  const outcomes = useSelector<InspectorState, ReactorOutcome[]>((s) =>
    correlationId ? s.outcomes[correlationId] ?? [] : [],
  );
  const flowData = useSelector<InspectorState, InspectorEvent[]>((s) => s.flowData);
  const scrubberPosition = useSelector<InspectorState, number | null>((s) => s.scrubberPosition);

  const { bars, minMs, maxMs } = useMemo(() => buildBars(outcomes), [outcomes]);
  const rangeMs = maxMs - minMs || 1;

  // Map event id → seq for scrubber sync
  const eventIdToSeq = useMemo(() => {
    const map = new Map<string, number>();
    for (const e of flowData) {
      if (e.id) map.set(e.id, e.seq);
    }
    return map;
  }, [flowData]);

  // Map scrubber seq → timestamp for cursor line
  const scrubberMs = useMemo(() => {
    if (scrubberPosition == null) return null;
    const event = flowData.find((e) => e.seq === scrubberPosition);
    if (!event) return null;
    return new Date(event.ts).getTime();
  }, [scrubberPosition, flowData]);

  if (!correlationId) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#71717a", fontSize: 13 }}>
        Open a flow to see the reactor waterfall
      </div>
    );
  }

  if (bars.length === 0) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#71717a", fontSize: 13 }}>
        No reactor execution data for this correlation
      </div>
    );
  }

  return (
    <div style={{ height: "100%", overflow: "auto", padding: `8px ${PADDING_X}px` }}>
      {/* Time axis */}
      <div style={{ display: "flex", marginBottom: 4, marginLeft: LABEL_WIDTH, position: "relative", height: 16 }}>
        <span style={{ fontSize: 9, color: "#52525b", position: "absolute", left: 0 }}>0ms</span>
        <span style={{ fontSize: 9, color: "#52525b", position: "absolute", right: 0 }}>
          {formatDuration(rangeMs)}
        </span>
        {scrubberMs != null && scrubberMs >= minMs && scrubberMs <= maxMs && (
          <div
            style={{
              position: "absolute",
              left: `${((scrubberMs - minMs) / rangeMs) * 100}%`,
              top: 0,
              bottom: -4,
              width: 1,
              background: "#3b82f6",
              pointerEvents: "none",
            }}
          />
        )}
      </div>

      {/* Bars */}
      {bars.map((bar) => {
        const offsetPct = ((bar.startMs - minMs) / rangeMs) * 100;
        const widthPct = Math.max(((bar.endMs - bar.startMs) / rangeMs) * 100, 0.5);
        const colors = statusColor(bar.status);
        const duration = bar.endMs - bar.startMs;

        // Scrubber sync: dim bars whose triggering events are all after scrubber
        const isFuture =
          scrubberPosition != null &&
          bar.triggeringEventIds.length > 0 &&
          bar.triggeringEventIds.every((eid) => {
            const seq = eventIdToSeq.get(eid);
            return seq != null && seq > scrubberPosition;
          });

        // Scrubber cursor position within this bar's track
        const cursorPct =
          scrubberMs != null && scrubberMs >= minMs && scrubberMs <= maxMs
            ? ((scrubberMs - minMs) / rangeMs) * 100
            : null;

        return (
          <div
            key={bar.reactorId}
            style={{
              display: "flex",
              alignItems: "center",
              height: ROW_HEIGHT,
              gap: 0,
              opacity: isFuture ? 0.3 : 1,
              transition: "opacity 150ms",
            }}
          >
            {/* Label */}
            <div
              style={{
                width: LABEL_WIDTH,
                flexShrink: 0,
                fontSize: 11,
                color: "#e4e4e7",
                fontWeight: 500,
                overflow: "hidden",
                textOverflow: "ellipsis",
                whiteSpace: "nowrap",
                paddingRight: 8,
              }}
              title={bar.reactorId}
            >
              {bar.reactorId}
            </div>

            {/* Bar track */}
            <div
              style={{
                flex: 1,
                position: "relative",
                height: 20,
                background: "#18181b",
                borderRadius: 4,
                overflow: "hidden",
              }}
            >
              {/* Bar */}
              <div
                style={{
                  position: "absolute",
                  left: `${offsetPct}%`,
                  width: `${widthPct}%`,
                  minWidth: BAR_MIN_WIDTH,
                  height: "100%",
                  background: colors.bar,
                  borderRadius: 3,
                  opacity: 0.85,
                  display: "flex",
                  alignItems: "center",
                  paddingLeft: 4,
                  paddingRight: 4,
                }}
                title={`${bar.reactorId}: ${bar.status} (${formatDuration(duration)})${bar.attempts > 1 ? ` — ${bar.attempts} attempts` : ""}${bar.error ? `\nError: ${bar.error}` : ""}`}
              >
                <span
                  style={{
                    fontSize: 9,
                    fontWeight: 600,
                    color: "#09090b",
                    whiteSpace: "nowrap",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                  }}
                >
                  {formatDuration(duration)}
                </span>
              </div>

              {/* Scrubber cursor line */}
              {cursorPct != null && (
                <div
                  style={{
                    position: "absolute",
                    left: `${cursorPct}%`,
                    top: 0,
                    bottom: 0,
                    width: 1,
                    background: "#3b82f6",
                    pointerEvents: "none",
                  }}
                />
              )}
            </div>

            {/* Status + attempts */}
            <div
              style={{
                width: 80,
                flexShrink: 0,
                display: "flex",
                alignItems: "center",
                gap: 4,
                paddingLeft: 8,
              }}
            >
              <span
                style={{
                  fontSize: 10,
                  fontWeight: 500,
                  color: colors.text,
                }}
              >
                {bar.status}
              </span>
              {bar.attempts > 1 && (
                <span style={{ fontSize: 9, color: "#71717a" }}>
                  x{bar.attempts}
                </span>
              )}
            </div>
          </div>
        );
      })}

      {/* Total duration */}
      <div style={{ marginTop: 8, fontSize: 10, color: "#52525b", marginLeft: LABEL_WIDTH }}>
        Total wall time: {formatDuration(rangeMs)}
      </div>
    </div>
  );
}
