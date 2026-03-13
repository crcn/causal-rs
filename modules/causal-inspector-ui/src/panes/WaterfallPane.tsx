import { useMemo, useCallback } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent, ReactorOutcome } from "../types";
import { inScrubberRange } from "../utils";
import { X } from "lucide-react";

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

const STATUS_COLORS: Record<string, { bar: string; barEnd: string; text: string }> = {
  completed: { bar: "#22c55e", barEnd: "#16a34a", text: "#bbf7d0" },
  running: { bar: "#eab308", barEnd: "#ca8a04", text: "#fef08a" },
  error: { bar: "#ef4444", barEnd: "#dc2626", text: "#fecaca" },
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

const ROW_HEIGHT = 36;
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
  const scrubberStart = useSelector<InspectorState, number | null>((s) => s.scrubberStart);
  const scrubberEnd = useSelector<InspectorState, number | null>((s) => s.scrubberEnd);
  const logsFilter = useSelector<InspectorState, { reactorId: string | null }>((s) => s.logsFilter);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const handleBarClick = useCallback(
    (bar: WaterfallBar) => {
      dispatch({ type: "ui/handler_selected", payload: { reactorId: bar.reactorId } });
    },
    [dispatch],
  );

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

  // Map scrubber end seq → timestamp for cursor line
  const scrubberMs = useMemo(() => {
    if (scrubberEnd == null) return null;
    const event = flowData.find((e) => e.seq === scrubberEnd);
    if (!event) return null;
    return new Date(event.ts).getTime();
  }, [scrubberEnd, flowData]);

  if (!correlationId) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#50506a", fontSize: 12, letterSpacing: "0.03em" }}>
        Open a flow to see the reactor waterfall
      </div>
    );
  }

  if (bars.length === 0) {
    return (
      <div style={{ height: "100%", display: "flex", alignItems: "center", justifyContent: "center", color: "#50506a", fontSize: 12, letterSpacing: "0.03em" }}>
        No reactor execution data for this correlation
      </div>
    );
  }

  const hasReactorFilter = logsFilter.reactorId != null;

  return (
    <div style={{ height: "100%", display: "flex", flexDirection: "column" }}>
      {/* Selection indicator */}
      {hasReactorFilter && (
        <div style={{ display: "flex", alignItems: "center", gap: 6, padding: "5px 12px", borderBottom: "1px solid rgba(255,255,255,0.06)", flexShrink: 0, background: "rgba(15, 15, 20, 0.6)", backdropFilter: "blur(8px)" }}>
          <span style={{ fontSize: 10, color: "#818cf8", fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace" }}>
            {logsFilter.reactorId}
          </span>
          <button
            onClick={() => {
              dispatch({ type: "ui/flow_node_selected", payload: null });
            }}
            style={{ marginLeft: "auto", background: "none", border: "none", cursor: "pointer", color: "#70708a", padding: 2, borderRadius: 4, display: "flex", alignItems: "center" }}
            title="Clear selection"
          >
            <X size={12} />
          </button>
        </div>
      )}

      <div style={{ flex: 1, overflow: "auto", padding: `10px ${PADDING_X}px` }}>
      {/* Time axis */}
      <div style={{ display: "flex", marginBottom: 6, marginLeft: LABEL_WIDTH, position: "relative", height: 16 }}>
        <span style={{ fontSize: 9, color: "#40405a", letterSpacing: "0.04em" }}>0ms</span>
        <span style={{ fontSize: 9, color: "#40405a", position: "absolute", right: 0, letterSpacing: "0.04em" }}>
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
              background: "#6366f1",
              pointerEvents: "none",
              boxShadow: "0 0 4px rgba(99, 102, 241, 0.4)",
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

        // Scrubber sync: dim bars whose triggering events are all outside range
        const isFuture =
          (scrubberStart != null || scrubberEnd != null) &&
          bar.triggeringEventIds.length > 0 &&
          bar.triggeringEventIds.every((eid) => {
            const seq = eventIdToSeq.get(eid);
            return seq != null && !inScrubberRange(seq, scrubberStart, scrubberEnd);
          });

        // Scrubber cursor position within this bar's track
        const cursorPct =
          scrubberMs != null && scrubberMs >= minMs && scrubberMs <= maxMs
            ? ((scrubberMs - minMs) / rangeMs) * 100
            : null;

        const isSelected = logsFilter.reactorId === bar.reactorId;

        return (
          <div key={bar.reactorId}>
            <div
              onClick={() => handleBarClick(bar)}
              style={{
                display: "flex",
                alignItems: "center",
                height: ROW_HEIGHT,
                gap: 0,
                opacity: isFuture ? 0.25 : (hasReactorFilter && !isSelected) ? 0.35 : 1,
                transition: "opacity 200ms, background 150ms",
                cursor: "pointer",
                borderRadius: 6,
                background: isSelected ? "rgba(99, 102, 241, 0.15)" : "transparent",
                paddingLeft: 4,
                paddingRight: 4,
              }}
            >
              {/* Label */}
              <div
                style={{
                  width: LABEL_WIDTH,
                  flexShrink: 0,
                  fontSize: 11,
                  color: "#c0c0d0",
                  fontWeight: 500,
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                  paddingRight: 10,
                  letterSpacing: "0.01em",
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
                  height: 22,
                  background: "rgba(255, 255, 255, 0.02)",
                  borderRadius: 5,
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
                    background: `linear-gradient(90deg, ${colors.bar}, ${colors.barEnd})`,
                    borderRadius: 4,
                    opacity: 0.8,
                    display: "flex",
                    alignItems: "center",
                    paddingLeft: 5,
                    paddingRight: 5,
                    boxShadow: `0 1px 4px ${colors.bar}30`,
                  }}
                  title={`${bar.reactorId}: ${bar.status} (${formatDuration(duration)})${bar.attempts > 1 ? ` — ${bar.attempts} attempts` : ""}${bar.error ? `\nError: ${bar.error}` : ""}`}
                >
                  <span
                    style={{
                      fontSize: 9,
                      fontWeight: 600,
                      color: "#0a0a0f",
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
                      background: "#6366f1",
                      pointerEvents: "none",
                      boxShadow: "0 0 4px rgba(99, 102, 241, 0.3)",
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
                  paddingLeft: 10,
                }}
              >
                <span
                  style={{
                    fontSize: 10,
                    fontWeight: 500,
                    color: colors.text,
                    opacity: 0.8,
                  }}
                >
                  {bar.status}
                </span>
                {bar.attempts > 1 && (
                  <span style={{ fontSize: 9, color: "#50506a" }}>
                    x{bar.attempts}
                  </span>
                )}
              </div>
            </div>
            {/* Error message inline */}
            {bar.status === "error" && bar.error && (
              <div style={{
                marginLeft: LABEL_WIDTH + 4,
                marginTop: -2,
                marginBottom: 4,
                fontSize: 10,
                color: "#f87171",
                paddingLeft: 4,
                overflow: "hidden",
                textOverflow: "ellipsis",
                whiteSpace: "nowrap",
              }} title={bar.error}>
                {bar.error}
              </div>
            )}
          </div>
        );
      })}

      {/* Total duration */}
      <div style={{ marginTop: 10, fontSize: 10, color: "#40405a", marginLeft: LABEL_WIDTH, letterSpacing: "0.03em" }}>
        Total wall time: {formatDuration(rangeMs)}
      </div>
      </div>
    </div>
  );
}
