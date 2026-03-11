import { useState, useCallback, useEffect, useRef } from "react";
import { useSelector, useDispatch } from "../machine";
import type { AdminState } from "../state";
import type { AdminMachineEvent } from "../events";
import type { AdminEvent } from "../types";
import { FilterBar } from "../components/FilterBar";
import { CopyablePayload } from "../components/CopyablePayload";
import { eventTextColor, LAYER_COLORS } from "../theme";
import { formatTs, compactPayload } from "../utils";

function EventRow({
  event,
  isSelected,
  onClick,
  onFilterRun,
  onInvestigate,
}: {
  event: AdminEvent;
  isSelected: boolean;
  onClick: () => void;
  onFilterRun: (runId: string) => void;
  onInvestigate?: () => void;
}) {
  const [payloadOpen, setPayloadOpen] = useState(false);
  const layerColor = LAYER_COLORS[event.layer] ?? "bg-zinc-500/20 text-zinc-400";

  return (
    <div
      className={`group w-full text-left px-3 py-2 border-b border-border hover:bg-accent/30 transition-colors ${
        isSelected ? "bg-accent/50 ring-1 ring-blue-500/50" : ""
      }`}
    >
      <div onClick={onClick} role="button" tabIndex={0} className="w-full text-left cursor-pointer">
        <div className="flex items-center gap-2 min-w-0">
          <span className="text-[10px] font-mono text-muted-foreground w-12 shrink-0 text-right">
            {event.seq}
          </span>
          <span className="text-[10px] text-muted-foreground shrink-0 w-32">
            {formatTs(event.ts)}
          </span>
          <span className={`px-1.5 py-0.5 rounded text-[10px] font-medium shrink-0 ${layerColor}`}>
            {event.layer}
          </span>
          {event.runId && (
            <button
              onClick={(e) => { e.stopPropagation(); onFilterRun(event.runId!); }}
              className="px-1 py-0.5 rounded text-[10px] font-mono bg-purple-500/10 text-purple-400 hover:bg-purple-500/20 shrink-0 transition-colors"
              title={`Filter by run ${event.runId}`}
            >
              {event.runId.slice(0, 8)}
            </button>
          )}
          <span className="text-xs font-mono shrink-0" style={{ color: eventTextColor(event.name) }}>
            {event.name}
          </span>
          <button
            onClick={(e) => { e.stopPropagation(); setPayloadOpen((v) => !v); }}
            className="text-[10px] font-mono text-muted-foreground hover:text-foreground truncate text-left"
            title="Click to expand payload"
          >
            {event.summary ?? compactPayload(event.payload)}
          </button>
          {onInvestigate && (
            <button
              onClick={(e) => { e.stopPropagation(); onInvestigate(); }}
              className="opacity-0 group-hover:opacity-100 transition-opacity ml-auto p-1 rounded hover:bg-accent shrink-0 text-[10px] text-muted-foreground"
              title="Investigate"
            >
              ?
            </button>
          )}
        </div>
      </div>
      {payloadOpen && (
        <CopyablePayload payload={event.payload} className="mt-1 ml-14 max-h-64" />
      )}
    </div>
  );
}

function InfiniteScrollSentinel({ onVisible, loading }: { onVisible: () => void; loading: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const onVisibleRef = useRef(onVisible);
  onVisibleRef.current = onVisible;

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => { if (entry.isIntersecting) onVisibleRef.current(); },
      { rootMargin: "200px" },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  return (
    <div ref={ref} className="flex items-center justify-center py-3">
      {loading && <span className="text-[10px] text-muted-foreground">Loading...</span>}
    </div>
  );
}

export type TimelinePaneProps = {
  /** Optional callback when user wants to investigate an event. */
  onInvestigate?: (event: AdminEvent) => void;
};

export function TimelinePane({ onInvestigate }: TimelinePaneProps = {}) {
  const events = useSelector<AdminState, AdminEvent[]>((s) => s.events);
  const loading = useSelector<AdminState, boolean>((s) => s.loading);
  const hasMore = useSelector<AdminState, boolean>((s) => s.hasMore);
  const selectedSeq = useSelector<AdminState, number | null>((s) => s.selectedSeq);
  const dispatch = useDispatch<AdminMachineEvent>();

  const handleSelect = useCallback(
    (event: AdminEvent) => {
      dispatch({ type: "ui/event_selected", payload: { seq: event.seq } });
      if (event.runId) {
        dispatch({ type: "ui/flow_opened", payload: { runId: event.runId } });
      }
    },
    [dispatch]
  );

  const handleFilterRun = useCallback(
    (runId: string) => {
      dispatch({ type: "ui/filter_changed", payload: { runId } });
    },
    [dispatch]
  );

  const handleLoadMore = useCallback(() => {
    dispatch({ type: "ui/load_more_requested" });
  }, [dispatch]);

  return (
    <div className="flex flex-col h-full">
      <FilterBar />
      {loading && events.length === 0 ? (
        <div className="animate-pulse">
          {Array.from({ length: 12 }).map((_, i) => (
            <div key={i} className="flex items-center gap-2 px-3 py-2 border-b border-border">
              <div className="h-3 w-12 bg-muted rounded shrink-0" />
              <div className="h-3 w-32 bg-muted rounded shrink-0" />
              <div className="h-4 w-14 bg-muted rounded shrink-0" />
              <div className="h-3 bg-muted rounded flex-1" style={{ maxWidth: `${150 + (i * 37) % 200}px` }} />
            </div>
          ))}
        </div>
      ) : events.length === 0 ? (
        <div className="flex items-center justify-center h-32 text-sm text-muted-foreground">
          No events found
        </div>
      ) : (
        <div className="flex-1 overflow-y-auto">
          {events.map((event) => (
            <EventRow
              key={event.seq}
              event={event}
              isSelected={event.seq === selectedSeq}
              onClick={() => handleSelect(event)}
              onFilterRun={handleFilterRun}
              onInvestigate={onInvestigate ? () => onInvestigate(event) : undefined}
            />
          ))}
          {hasMore && (
            <InfiniteScrollSentinel onVisible={handleLoadMore} loading={loading} />
          )}
        </div>
      )}
    </div>
  );
}
