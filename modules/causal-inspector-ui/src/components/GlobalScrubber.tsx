import { useCallback, useMemo, useRef } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { FlowSelection } from "../types";
import { getScrubberSequence, inScrubberRange } from "../utils";
import { Play, Pause, SkipBack, SkipForward, ChevronsRight, RotateCcw } from "lucide-react";

// ---------------------------------------------------------------------------
// Dual-handle range slider
// ---------------------------------------------------------------------------

function RangeSlider({
  seqs,
  start,
  end,
  onStartChange,
  onEndChange,
}: {
  seqs: number[];
  start: number | null;
  end: number | null;
  onStartChange: (start: number | null) => void;
  onEndChange: (end: number | null) => void;
}) {
  const trackRef = useRef<HTMLDivElement>(null);
  const min = seqs[0] ?? 0;
  const max = seqs[seqs.length - 1] ?? 0;
  const range = max - min || 1;

  const startVal = start ?? min;
  const endVal = end ?? max;
  const leftPct = ((startVal - min) / range) * 100;
  const rightPct = ((endVal - min) / range) * 100;

  function snapToNearest(value: number): number {
    return seqs.reduce((best, s) =>
      Math.abs(s - value) < Math.abs(best - value) ? s : best, seqs[0]);
  }

  function valueFromPointer(clientX: number): number {
    const track = trackRef.current;
    if (!track) return min;
    const rect = track.getBoundingClientRect();
    const pct = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    return min + pct * range;
  }

  function handlePointerDown(which: "start" | "end") {
    return (e: React.PointerEvent) => {
      e.preventDefault();
      const el = e.currentTarget as HTMLElement;
      el.setPointerCapture(e.pointerId);

      const onMove = (ev: PointerEvent) => {
        const raw = valueFromPointer(ev.clientX);
        const snapped = snapToNearest(raw);
        if (which === "start") {
          // Clamp: start cannot exceed end
          const clampedEnd = end ?? max;
          onStartChange(snapped >= clampedEnd ? clampedEnd : snapped <= min ? null : snapped);
        } else {
          // Clamp: end cannot go below start
          const clampedStart = start ?? min;
          onEndChange(snapped <= clampedStart ? clampedStart : snapped >= max ? null : snapped);
        }
      };

      const onUp = () => {
        el.removeEventListener("pointermove", onMove);
        el.removeEventListener("pointerup", onUp);
      };

      el.addEventListener("pointermove", onMove);
      el.addEventListener("pointerup", onUp);
    };
  }

  return (
    <div ref={trackRef} className="relative flex-1 h-5 flex items-center cursor-pointer group/slider">
      {/* Track background */}
      <div className="absolute inset-x-0 h-1 rounded-full bg-white/[0.06]" />
      {/* Active range highlight */}
      <div
        className="absolute h-1 rounded-full bg-indigo-500/40 transition-none"
        style={{ left: `${leftPct}%`, width: `${rightPct - leftPct}%` }}
      />
      {/* Start handle */}
      <div
        onPointerDown={handlePointerDown("start")}
        className="absolute w-3 h-3 rounded-full bg-white/60 hover:bg-white border border-white/20 hover:border-indigo-400/50 shadow-sm transition-colors duration-100 -translate-x-1/2 cursor-grab active:cursor-grabbing z-10"
        style={{ left: `${leftPct}%` }}
        title="Range start"
      />
      {/* End handle */}
      <div
        onPointerDown={handlePointerDown("end")}
        className="absolute w-3 h-3 rounded-full bg-indigo-400 hover:bg-indigo-300 border border-indigo-400/50 hover:border-indigo-300 shadow-sm transition-colors duration-100 -translate-x-1/2 cursor-grab active:cursor-grabbing z-10"
        style={{ left: `${rightPct}%` }}
        title="Range end"
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Context label
// ---------------------------------------------------------------------------

function getContextLabel(
  flowCorrelationId: string | null,
  flowSelection: FlowSelection,
): string {
  if (!flowCorrelationId) return "All events";
  if (flowSelection) {
    if (flowSelection.kind === "event-type") return `${flowSelection.name} events`;
    return `Reactor ${flowSelection.reactorId}`;
  }
  return `Flow ${flowCorrelationId.slice(0, 8)}`;
}

// ---------------------------------------------------------------------------
// GlobalScrubber
// ---------------------------------------------------------------------------

export function GlobalScrubber() {
  const scrubberStart = useSelector<InspectorState, number | null>((s) => s.scrubberStart);
  const scrubberEnd = useSelector<InspectorState, number | null>((s) => s.scrubberEnd);
  const playing = useSelector<InspectorState, boolean>((s) => s.scrubberPlaying);
  const speed = useSelector<InspectorState, number>((s) => s.scrubberSpeed);
  const flowCorrelationId = useSelector<InspectorState, string | null>((s) => s.flowCorrelationId);
  const flowSelection = useSelector<InspectorState, FlowSelection>((s) => s.flowSelection);
  const flowData = useSelector<InspectorState, InspectorState["flowData"]>((s) => s.flowData);
  const events = useSelector<InspectorState, InspectorState["events"]>((s) => s.events);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const seqs = useMemo(
    () => getScrubberSequence({ flowCorrelationId, flowData, flowSelection, events } as InspectorState),
    [flowCorrelationId, flowData, flowSelection, events],
  );

  const endVal = scrubberEnd ?? (seqs[seqs.length - 1] ?? 0);
  const isAtEnd = scrubberEnd == null || (seqs.length > 0 && scrubberEnd >= seqs[seqs.length - 1]);

  // Count events in range
  const startIndex = scrubberStart != null
    ? seqs.filter((s) => s < scrubberStart).length + 1
    : 1;
  const endIndex = scrubberEnd != null
    ? seqs.filter((s) => s <= scrubberEnd).length
    : seqs.length;

  const hasRange = scrubberStart != null;

  const stepBack = useCallback(() => {
    const idx = seqs.findIndex((s) => s >= endVal);
    const prev = seqs[Math.max(0, idx - 1)];
    if (prev != null) dispatch({ type: "ui/scrubber_end_changed", payload: { end: prev } });
  }, [seqs, endVal, dispatch]);

  const stepForward = useCallback(() => {
    const next = seqs.find((s) => s > endVal);
    if (next != null) {
      dispatch({ type: "ui/scrubber_end_changed", payload: { end: next } });
    } else {
      dispatch({ type: "ui/scrubber_end_changed", payload: { end: null } });
    }
  }, [seqs, endVal, dispatch]);

  const reset = useCallback(() => {
    if (playing) dispatch({ type: "ui/scrubber_play_toggled" });
    dispatch({ type: "ui/scrubber_start_changed", payload: { start: null } });
    dispatch({ type: "ui/scrubber_end_changed", payload: { end: seqs[0] ?? null } });
  }, [seqs, playing, dispatch]);

  const jumpToEnd = useCallback(() => {
    if (playing) dispatch({ type: "ui/scrubber_play_toggled" });
    dispatch({ type: "ui/scrubber_end_changed", payload: { end: null } });
  }, [playing, dispatch]);

  const togglePlay = useCallback(() => {
    if (!playing && isAtEnd && seqs.length > 0) {
      dispatch({ type: "ui/scrubber_end_changed", payload: { end: seqs[0] } });
    }
    dispatch({ type: "ui/scrubber_play_toggled" });
  }, [playing, isAtEnd, seqs, dispatch]);

  const cycleSpeed = useCallback(() => {
    const speeds = [500, 300, 150, 50];
    const idx = speeds.indexOf(speed);
    const next = speeds[(idx + 1) % speeds.length];
    dispatch({ type: "ui/scrubber_speed_changed", payload: { speed: next } });
  }, [speed, dispatch]);

  const handleStartChange = useCallback((start: number | null) => {
    dispatch({ type: "ui/scrubber_start_changed", payload: { start } });
  }, [dispatch]);

  const handleEndChange = useCallback((end: number | null) => {
    dispatch({ type: "ui/scrubber_end_changed", payload: { end } });
  }, [dispatch]);

  const disabled = seqs.length < 2;
  const speedLabel = speed <= 50 ? "4x" : speed <= 150 ? "2x" : speed <= 300 ? "1x" : "0.5x";
  const btnClass = `p-1.5 rounded-md transition-all duration-150 ${disabled ? "text-muted-foreground/20 cursor-default" : "text-muted-foreground/60 hover:text-foreground hover:bg-white/[0.05]"}`;
  const contextLabel = getContextLabel(flowCorrelationId, flowSelection);

  return (
    <div
      className="flex items-center gap-2 px-3 py-2 border-t border-border shrink-0"
      style={{ background: "rgba(15, 15, 20, 0.6)", backdropFilter: "blur(8px)" }}
    >
      {/* Context label */}
      <span className="text-[9px] font-medium text-muted-foreground/40 uppercase tracking-wider shrink-0 max-w-[120px] truncate" title={contextLabel}>
        {contextLabel}
      </span>

      {/* Transport controls */}
      <button onClick={disabled ? undefined : reset} className={btnClass} title="Reset to start">
        <RotateCcw size={12} />
      </button>
      <button onClick={disabled ? undefined : stepBack} className={btnClass} title="Step back">
        <SkipBack size={12} />
      </button>
      <button
        onClick={disabled ? undefined : togglePlay}
        className={`p-1.5 rounded-md transition-all duration-150 ${
          disabled
            ? "text-muted-foreground/20 cursor-default"
            : playing
              ? "text-indigo-400 bg-indigo-500/10"
              : "text-muted-foreground/60 hover:text-foreground hover:bg-white/[0.05]"
        }`}
        title={playing ? "Pause" : "Play"}
      >
        {playing ? <Pause size={14} /> : <Play size={14} />}
      </button>
      <button onClick={disabled ? undefined : stepForward} className={btnClass} title="Step forward">
        <SkipForward size={12} />
      </button>
      <button onClick={disabled ? undefined : jumpToEnd} className={btnClass} title="Jump to end">
        <ChevronsRight size={12} />
      </button>

      {/* Range slider */}
      {disabled ? (
        <div className="flex-1 h-5 flex items-center">
          <div className="w-full h-1 rounded-full bg-white/[0.04]" />
        </div>
      ) : (
        <RangeSlider
          seqs={seqs}
          start={scrubberStart}
          end={scrubberEnd}
          onStartChange={handleStartChange}
          onEndChange={handleEndChange}
        />
      )}

      {/* Counter */}
      <span className={`text-[10px] tabular-nums min-w-[4rem] text-right shrink-0 ${disabled ? "text-muted-foreground/20" : "text-muted-foreground/60"}`}>
        {disabled ? `${seqs.length}` : `${hasRange ? `${startIndex}-${endIndex}` : endIndex}/${seqs.length}`}
      </span>

      {/* Speed toggle */}
      <button
        onClick={disabled ? undefined : cycleSpeed}
        className={`text-[10px] px-2 py-1 rounded-md border tabular-nums min-w-[2.5rem] text-center transition-all duration-150 shrink-0 ${disabled ? "text-muted-foreground/20 border-border/50 cursor-default" : "text-muted-foreground/60 hover:text-foreground border-border hover:border-indigo-500/30"}`}
        title="Playback speed"
      >
        {speedLabel}
      </button>
    </div>
  );
}
