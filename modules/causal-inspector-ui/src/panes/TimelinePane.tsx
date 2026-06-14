import { useState, useCallback, useEffect, useRef, useMemo } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent, InspectorEffect } from "../types";
import { FilterBar } from "../components/FilterBar";
import { CopyablePayload } from "../components/CopyablePayload";
import { EffectList } from "../components/EffectList";
import { eventTextColor, eventBg } from "../theme";
import { formatTs, compactPayload, inScrubberRange } from "../utils";
import { Search, ChevronRight, Zap } from "lucide-react";

function EventRow({
  event,
  isSelected,
  onClick,
  onFilterWorkflow,
  onInvestigate,
  onSubjectSelected,
  onEffectsRequested,
  effects,
  loadingEffects,
}: {
  event: InspectorEvent;
  isSelected: boolean;
  onClick: () => void;
  onFilterWorkflow: (workflowId: string) => void;
  onInvestigate?: () => void;
  onSubjectSelected?: (aggregateType: string, aggregateId: string) => void;
  onEffectsRequested?: (eventId: string) => void;
  effects?: InspectorEffect[];
  loadingEffects?: boolean;
}) {
  const [payloadOpen, setPayloadOpen] = useState(false);
  const [effectsOpen, setEffectsOpen] = useState(false);

  const handleEffectsToggle = (e: React.MouseEvent) => {
    e.stopPropagation();
    setEffectsOpen((v) => !v);
  };

  return (
    <div
      className={`group w-full text-left px-3 py-2 border-b border-border transition-all duration-150 ${
        isSelected ? "bg-indigo-500/15" : "hover:bg-white/[0.02]"
      }`}
    >
      <div onClick={onClick} role="button" tabIndex={0} className="w-full text-left cursor-pointer">
        <div className="flex items-center gap-2.5 min-w-0">
          <span className="text-[10px] font-mono text-muted-foreground/60 w-10 shrink-0 text-right tabular-nums">
            {event.seq}
          </span>
          <span className="text-[10px] text-muted-foreground/70 shrink-0 w-32 tabular-nums">
            {formatTs(event.ts)}
          </span>
          {event.workflowId && (
            <button
              onClick={(e) => { e.stopPropagation(); onFilterWorkflow(event.workflowId!); }}
              className="px-1.5 py-0.5 rounded-full text-[9px] font-mono bg-purple-500/8 text-purple-400/80 hover:bg-purple-500/15 hover:text-purple-400 shrink-0 transition-all border border-purple-500/10"
              title={`Filter by workflow ${event.workflowId}`}
            >
              {event.workflowId.slice(0, 8)}
            </button>
          )}
          {event.aggregateType && event.aggregateId && onSubjectSelected && (
            <button
              onClick={(e) => { e.stopPropagation(); onSubjectSelected(event.aggregateType!, event.aggregateId!); }}
              className="px-1.5 py-0.5 rounded-full text-[9px] font-mono bg-teal-500/8 text-teal-400/80 hover:bg-teal-500/15 hover:text-teal-400 shrink-0 transition-all border border-teal-500/10"
              title={`View subject ${event.aggregateType}:${event.aggregateId}`}
            >
              {event.aggregateType}:{event.aggregateId.slice(0, 8)}
            </button>
          )}
          <span
            className="text-xs font-mono shrink-0 px-1.5 py-0.5 rounded"
            style={{ color: eventTextColor(event.name), background: eventBg(event.name) }}
          >
            {event.name}
          </span>
          <button
            onClick={(e) => { e.stopPropagation(); setPayloadOpen((v) => !v); }}
            className="flex items-center gap-1 text-[10px] font-mono text-muted-foreground/60 hover:text-muted-foreground truncate text-left min-w-0 transition-colors"
            title="Click to expand payload"
          >
            <ChevronRight size={10} className={`shrink-0 transition-transform duration-150 ${payloadOpen ? "rotate-90" : ""}`} />
            <span className="truncate">{event.summary ?? compactPayload(event.payload)}</span>
          </button>
          <div className="ml-auto flex items-center gap-1 shrink-0">
            {event.id && (
              <button
                onClick={(e) => {
                  handleEffectsToggle(e);
                  if (!effectsOpen && effects === undefined && event.id) {
                    onEffectsRequested?.(event.id);
                  }
                }}
                className={`opacity-0 group-hover:opacity-100 transition-all duration-150 flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] ${
                  effectsOpen
                    ? "opacity-100 bg-indigo-500/10 text-indigo-400/70 border border-indigo-500/20"
                    : "hover:bg-white/[0.05] text-muted-foreground/50 border border-transparent"
                }`}
                title="Show effects"
              >
                <Zap size={9} />
                {effects !== undefined && effects.length > 0 && <span>{effects.length}</span>}
              </button>
            )}
            {onInvestigate && (
              <button
                onClick={(e) => { e.stopPropagation(); onInvestigate(); }}
                className="opacity-0 group-hover:opacity-100 transition-opacity duration-150 p-1 rounded-md hover:bg-white/[0.05] shrink-0 text-muted-foreground"
                title="Investigate"
              >
                <Search size={12} />
              </button>
            )}
          </div>
        </div>
      </div>
      {payloadOpen && (
        <CopyablePayload payload={event.payload} className="mt-2 ml-12 max-h-64" />
      )}
      {effectsOpen && (
        <div className="mt-1 ml-12">
          {loadingEffects ? (
            <div className="text-[9px] text-muted-foreground/40 italic py-1">Loading…</div>
          ) : (
            <EffectList effects={effects ?? []} />
          )}
        </div>
      )}
    </div>
  );
}

function InfiniteScrollSentinel({ onVisible, loading }: { onVisible: () => void; loading: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const onVisibleRef = useRef(onVisible);
  onVisibleRef.current = onVisible;

  useEffect(() => {
    if (loading) return;
    const el = ref.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => { if (entry.isIntersecting) onVisibleRef.current(); },
      { rootMargin: "200px" },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [loading]);

  return (
    <div ref={ref} className="flex items-center justify-center py-4">
      {loading && (
        <div className="flex items-center gap-2">
          <div className="w-1.5 h-1.5 rounded-full bg-indigo-500/50 animate-pulse" />
          <span className="text-[10px] text-muted-foreground/60">Loading</span>
        </div>
      )}
    </div>
  );
}

export type TimelinePaneProps = {
  /** Optional callback when user wants to investigate an event. */
  onInvestigate?: (event: InspectorEvent) => void;
};

export function TimelinePane({ onInvestigate }: TimelinePaneProps = {}) {
  const events = useSelector<InspectorState, InspectorEvent[]>((s) => {
    let result = s.events;
    const cid = s.filters.workflowId;
    if (cid) result = result.filter((e) => e.workflowId === cid);
    const search = s.filters.search?.toLowerCase();
    if (search) {
      result = result.filter((e) =>
        e.name.toLowerCase().includes(search) ||
        e.payload.toLowerCase().includes(search) ||
        (e.workflowId ?? "").toLowerCase().includes(search)
      );
    }
    return result;
  });
  const loading = useSelector<InspectorState, boolean>((s) => s.loading);
  const hasMore = useSelector<InspectorState, boolean>((s) => s.hasMore);
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const scrubberStart = useSelector<InspectorState, number | null>((s) => s.scrubberStart);
  const scrubberEnd = useSelector<InspectorState, number | null>((s) => s.scrubberEnd);
  const expandedEffects = useSelector<InspectorState, Record<string, InspectorEffect[]>>((s) => s.expandedEffects);
  const loadingEffectsIds = useSelector<InspectorState, string[]>((s) => s.loadingEffects);
  const dispatch = useDispatch<InspectorMachineEvent>();

  const displayedEvents = useMemo(() => {
    if (scrubberStart == null && scrubberEnd == null) return events;
    return events.filter(e => inScrubberRange(e.seq, scrubberStart, scrubberEnd));
  }, [events, scrubberStart, scrubberEnd]);

  const handleSelect = useCallback(
    (event: InspectorEvent) => {
      dispatch({ type: "ui/event_selected", payload: { seq: event.seq } });
      if (event.workflowId) {
        dispatch({ type: "ui/flow_opened", payload: { workflowId: event.workflowId } });
      }
    },
    [dispatch]
  );

  const handleFilterWorkflow = useCallback(
    (workflowId: string) => {
      dispatch({ type: "ui/filter_changed", payload: { workflowId } });
    },
    [dispatch]
  );

  const handleLoadMore = useCallback(() => {
    dispatch({ type: "ui/load_more_requested" });
  }, [dispatch]);

  const handleSubjectSelected = useCallback(
    (aggregateType: string, aggregateId: string) => {
      dispatch({ type: "ui/subject_selected", payload: { aggregateType, aggregateId, mode: "both" } });
    },
    [dispatch]
  );

  const handleEffectsRequested = useCallback(
    (eventId: string) => {
      dispatch({ type: "ui/event_effects_requested", payload: { eventId } });
    },
    [dispatch]
  );

  return (
    <div className="flex flex-col h-full">
      <FilterBar />
      {loading && events.length === 0 ? (
        <div className="animate-pulse p-1">
          {Array.from({ length: 12 }).map((_, i) => (
            <div key={i} className="flex items-center gap-2 px-3 py-2.5 border-b border-border">
              <div className="h-3 w-10 bg-white/[0.03] rounded shrink-0" />
              <div className="h-3 w-32 bg-white/[0.03] rounded shrink-0" />
              <div className="h-3 bg-white/[0.03] rounded flex-1" style={{ maxWidth: `${150 + (i * 37) % 200}px` }} />
            </div>
          ))}
        </div>
      ) : events.length === 0 ? (
        <div className="flex items-center justify-center h-32 text-sm text-muted-foreground/60">
          No events found
        </div>
      ) : (
        <div className="flex-1 overflow-y-auto">
          {displayedEvents.map((event) => (
            <EventRow
              key={event.seq}
              event={event}
              isSelected={event.seq === selectedSeq}
              onClick={() => handleSelect(event)}
              onFilterWorkflow={handleFilterWorkflow}
              onInvestigate={onInvestigate ? () => onInvestigate(event) : undefined}
              onSubjectSelected={handleSubjectSelected}
              onEffectsRequested={handleEffectsRequested}
              effects={event.id ? expandedEffects[event.id] : undefined}
              loadingEffects={event.id ? loadingEffectsIds.includes(event.id) : false}
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
