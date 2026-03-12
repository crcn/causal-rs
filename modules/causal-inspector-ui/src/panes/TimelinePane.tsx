import { useState, useCallback, useEffect, useRef } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent } from "../types";
import { FilterBar } from "../components/FilterBar";
import { CopyablePayload } from "../components/CopyablePayload";
import { eventTextColor } from "../theme";
import { formatTs, compactPayload } from "../utils";
import { Search, ChevronRight } from "lucide-react";

function EventRow({
  event,
  isSelected,
  onClick,
  onFilterCorrelation,
  onInvestigate,
}: {
  event: InspectorEvent;
  isSelected: boolean;
  onClick: () => void;
  onFilterCorrelation: (correlationId: string) => void;
  onInvestigate?: () => void;
}) {
  const [payloadOpen, setPayloadOpen] = useState(false);

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
          {event.correlationId && (
            <button
              onClick={(e) => { e.stopPropagation(); onFilterCorrelation(event.correlationId!); }}
              className="px-1 py-0.5 rounded text-[10px] font-mono bg-purple-500/10 text-purple-400 hover:bg-purple-500/20 shrink-0 transition-colors"
              title={`Filter by correlation ${event.correlationId}`}
            >
              {event.correlationId.slice(0, 8)}
            </button>
          )}
          <span className="text-xs font-mono shrink-0" style={{ color: eventTextColor(event.name) }}>
            {event.name}
          </span>
          <button
            onClick={(e) => { e.stopPropagation(); setPayloadOpen((v) => !v); }}
            className="flex items-center gap-1 text-[10px] font-mono text-muted-foreground hover:text-foreground truncate text-left min-w-0"
            title="Click to expand payload"
          >
            <ChevronRight size={10} className={`shrink-0 transition-transform ${payloadOpen ? "rotate-90" : ""}`} />
            <span className="truncate">{event.summary ?? compactPayload(event.payload)}</span>
          </button>
          {onInvestigate && (
            <button
              onClick={(e) => { e.stopPropagation(); onInvestigate(); }}
              className="opacity-0 group-hover:opacity-100 transition-opacity ml-auto p-1 rounded hover:bg-accent shrink-0 text-muted-foreground"
              title="Investigate"
            >
              <Search size={12} />
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
  onInvestigate?: (event: InspectorEvent) => void;
};

export function TimelinePane({ onInvestigate }: TimelinePaneProps = {}) {
  const events = useSelector<InspectorState, InspectorEvent[]>((s) => s.events);
  const loading = useSelector<InspectorState, boolean>((s) => s.loading);
  const hasMore = useSelector<InspectorState, boolean>((s) => s.hasMore);
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const handleSelect = useCallback(
    (event: InspectorEvent) => {
      dispatch({ type: "ui/event_selected", payload: { seq: event.seq } });
      if (event.correlationId) {
        dispatch({ type: "ui/flow_opened", payload: { correlationId: event.correlationId } });
      }
    },
    [dispatch]
  );

  const handleFilterCorrelation = useCallback(
    (correlationId: string) => {
      dispatch({ type: "ui/filter_changed", payload: { correlationId } });
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
              onFilterCorrelation={handleFilterCorrelation}
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
