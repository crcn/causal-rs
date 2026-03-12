import { useState, useCallback, useEffect, useRef, useMemo } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { InspectorEvent } from "../types";
import { FilterBar } from "../components/FilterBar";
import { CopyablePayload } from "../components/CopyablePayload";
import { eventTextColor, eventBg, eventBorder } from "../theme";
import { formatTs, compactPayload } from "../utils";
import { Search, ChevronRight, ChevronDown } from "lucide-react";

function EventRow({
  event,
  isSelected,
  onClick,
  onFilterCorrelation,
  onInvestigate,
  indent,
}: {
  event: InspectorEvent;
  isSelected: boolean;
  onClick: () => void;
  onFilterCorrelation: (correlationId: string) => void;
  onInvestigate?: () => void;
  indent?: boolean;
}) {
  const [payloadOpen, setPayloadOpen] = useState(false);

  return (
    <div
      className={`group w-full text-left px-3 py-2 border-b border-border transition-all duration-150 ${
        isSelected
          ? "bg-indigo-500/15"
          : "hover:bg-white/[0.02]"
      }`}
      style={indent ? { paddingLeft: 28 } : undefined}
    >
      <div onClick={onClick} role="button" tabIndex={0} className="w-full text-left cursor-pointer">
        <div className="flex items-center gap-2.5 min-w-0">
          <span className="text-[10px] font-mono text-muted-foreground/60 w-10 shrink-0 text-right tabular-nums">
            {event.seq}
          </span>
          <span className="text-[10px] text-muted-foreground/70 shrink-0 w-32 tabular-nums">
            {formatTs(event.ts)}
          </span>
          {event.correlationId && (
            <button
              onClick={(e) => { e.stopPropagation(); onFilterCorrelation(event.correlationId!); }}
              className="px-1.5 py-0.5 rounded-full text-[9px] font-mono bg-purple-500/8 text-purple-400/80 hover:bg-purple-500/15 hover:text-purple-400 shrink-0 transition-all border border-purple-500/10"
              title={`Filter by correlation ${event.correlationId}`}
            >
              {event.correlationId.slice(0, 8)}
            </button>
          )}
          <span
            className="text-xs font-mono shrink-0 px-1.5 py-0.5 rounded"
            style={{
              color: eventTextColor(event.name),
              background: eventBg(event.name),
            }}
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
          {onInvestigate && (
            <button
              onClick={(e) => { e.stopPropagation(); onInvestigate(); }}
              className="opacity-0 group-hover:opacity-100 transition-opacity duration-150 ml-auto p-1 rounded-md hover:bg-white/[0.05] shrink-0 text-muted-foreground"
              title="Investigate"
            >
              <Search size={12} />
            </button>
          )}
        </div>
      </div>
      {payloadOpen && (
        <CopyablePayload payload={event.payload} className="mt-2 ml-12 max-h-64" />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Event grouping by correlation
// ---------------------------------------------------------------------------

type EventGroup = {
  correlationId: string;
  root: InspectorEvent;
  children: InspectorEvent[];
};

function groupEventsByCorrelation(events: InspectorEvent[]): (InspectorEvent | EventGroup)[] {
  const result: (InspectorEvent | EventGroup)[] = [];
  let i = 0;

  while (i < events.length) {
    const event = events[i];

    if (!event.correlationId) {
      result.push(event);
      i++;
      continue;
    }

    // Collect consecutive events with the same correlationId
    const cid = event.correlationId;
    const groupEvents: InspectorEvent[] = [event];
    let j = i + 1;
    while (j < events.length && events[j].correlationId === cid) {
      groupEvents.push(events[j]);
      j++;
    }

    if (groupEvents.length === 1) {
      // Single event — render flat
      result.push(event);
    } else {
      // Find the root event (no reactorId) or fall back to first
      const rootIdx = groupEvents.findIndex((e) => !e.reactorId);
      const root = rootIdx >= 0 ? groupEvents[rootIdx] : groupEvents[0];
      const children = groupEvents.filter((e) => e !== root);
      result.push({ correlationId: cid, root, children });
    }

    i = j;
  }

  return result;
}

function EventGroupRow({
  group,
  selectedSeq,
  collapsed,
  onToggle,
  onSelect,
  onFilterCorrelation,
  onInvestigate,
}: {
  group: EventGroup;
  selectedSeq: number | null;
  collapsed: boolean;
  onToggle: () => void;
  onSelect: (event: InspectorEvent) => void;
  onFilterCorrelation: (correlationId: string) => void;
  onInvestigate?: (event: InspectorEvent) => void;
}) {
  return (
    <div>
      {/* Group header */}
      <div className="relative">
        <button
          onClick={onToggle}
          className="absolute left-1 top-2.5 z-10 p-0.5 rounded hover:bg-white/[0.05] text-muted-foreground/60 hover:text-muted-foreground transition-colors"
          title={collapsed ? "Expand group" : "Collapse group"}
        >
          {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
        </button>
        <div className="relative">
          <EventRow
            event={group.root}
            isSelected={group.root.seq === selectedSeq}
            onClick={() => onSelect(group.root)}
            onFilterCorrelation={onFilterCorrelation}
            onInvestigate={onInvestigate ? () => onInvestigate(group.root) : undefined}
          />
          {collapsed && (
            <span
              className="absolute right-3 top-2.5 text-[9px] font-mono px-1.5 py-0.5 rounded-full bg-white/[0.04] text-muted-foreground/60 border border-border"
            >
              +{group.children.length}
            </span>
          )}
        </div>
      </div>

      {/* Child events */}
      {!collapsed &&
        group.children.map((child) => (
          <EventRow
            key={child.seq}
            event={child}
            isSelected={child.seq === selectedSeq}
            onClick={() => onSelect(child)}
            onFilterCorrelation={onFilterCorrelation}
            onInvestigate={onInvestigate ? () => onInvestigate(child) : undefined}
            indent
          />
        ))}
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
  const events = useSelector<InspectorState, InspectorEvent[]>((s) => s.events);
  const loading = useSelector<InspectorState, boolean>((s) => s.loading);
  const hasMore = useSelector<InspectorState, boolean>((s) => s.hasMore);
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const dispatch = useDispatch<InspectorMachineEvent>();

  // Default collapsed — this set tracks which groups are *expanded*
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(() => new Set());

  const grouped = useMemo(() => groupEventsByCorrelation(events), [events]);

  const toggleGroup = useCallback((correlationId: string) => {
    setExpandedGroups((prev) => {
      const next = new Set(prev);
      if (next.has(correlationId)) next.delete(correlationId);
      else next.add(correlationId);
      return next;
    });
  }, []);

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
          {grouped.map((item) => {
            if ("correlationId" in item && "root" in item) {
              // EventGroup
              const group = item as EventGroup;
              return (
                <EventGroupRow
                  key={`group-${group.correlationId}-${group.root.seq}`}
                  group={group}
                  selectedSeq={selectedSeq}
                  collapsed={!expandedGroups.has(group.correlationId)}
                  onToggle={() => toggleGroup(group.correlationId)}
                  onSelect={handleSelect}
                  onFilterCorrelation={handleFilterCorrelation}
                  onInvestigate={onInvestigate}
                />
              );
            }
            // Single event
            const event = item as InspectorEvent;
            return (
              <EventRow
                key={event.seq}
                event={event}
                isSelected={event.seq === selectedSeq}
                onClick={() => handleSelect(event)}
                onFilterCorrelation={handleFilterCorrelation}
                onInvestigate={onInvestigate ? () => onInvestigate(event) : undefined}
              />
            );
          })}
          {hasMore && (
            <InfiniteScrollSentinel onVisible={handleLoadMore} loading={loading} />
          )}
        </div>
      )}
    </div>
  );
}
