import { useState } from "react";
import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import type { SubjectChainEvent, SubjectChainMode, InspectorEffect } from "../types";
import { CopyablePayload } from "../components/CopyablePayload";
import { EffectList } from "../components/EffectList";
import { eventTextColor, eventBg } from "../theme";
import { formatTs, compactPayload } from "../utils";
import { AlertTriangle, ChevronRight, Zap } from "lucide-react";

// ── SubjectChainEventRow ──────────────────────────────────────────────────

function SubjectChainEventRow({
  event,
  showSourceBadge,
  effects,
  loadingEffects,
  dispatch,
}: {
  event: SubjectChainEvent;
  showSourceBadge: boolean;
  effects: InspectorEffect[] | undefined;
  loadingEffects: boolean;
  dispatch: (e: InspectorMachineEvent) => void;
}) {
  const [payloadOpen, setPayloadOpen] = useState(false);
  const [effectsOpen, setEffectsOpen] = useState(false);

  const handleEffectsToggle = (e: React.MouseEvent) => {
    e.stopPropagation();
    if (!effectsOpen && effects === undefined && event.id) {
      dispatch({ type: "ui/event_effects_requested", payload: { eventId: event.id } });
    }
    setEffectsOpen((v) => !v);
  };

  return (
    <div className="group px-3 py-2 border-b border-border hover:bg-white/[0.02] transition-all duration-150">
      <div className="flex items-center gap-2 min-w-0">
        <span className="text-[10px] font-mono text-muted-foreground/60 w-10 shrink-0 text-right tabular-nums">
          {event.seq}
        </span>
        <span className="text-[10px] text-muted-foreground/70 shrink-0 w-28 tabular-nums">
          {formatTs(event.ts)}
        </span>
        {showSourceBadge && (
          <span
            className={`px-1.5 py-0.5 rounded text-[8px] font-medium shrink-0 border ${
              event.sourceMode === "stream"
                ? "bg-indigo-500/10 text-indigo-400/80 border-indigo-500/20"
                : "bg-zinc-500/10 text-zinc-400/60 border-zinc-500/15"
            }`}
          >
            {event.sourceMode}
          </span>
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
        >
          <ChevronRight size={10} className={`shrink-0 transition-transform duration-150 ${payloadOpen ? "rotate-90" : ""}`} />
          <span className="truncate">{event.summary ?? compactPayload(event.payload)}</span>
        </button>
        {event.id && (
          <button
            onClick={handleEffectsToggle}
            className={`ml-auto opacity-0 group-hover:opacity-100 transition-all duration-150 flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] shrink-0 ${
              effectsOpen
                ? "opacity-100 bg-indigo-500/10 text-indigo-400/70 border border-indigo-500/20"
                : "hover:bg-white/[0.05] text-muted-foreground/50 border border-transparent"
            }`}
            title="Show effects"
          >
            <Zap size={9} />
            {effects !== undefined && effects.length > 0 && (
              <span>{effects.length}</span>
            )}
          </button>
        )}
      </div>

      {payloadOpen && (
        <CopyablePayload payload={event.payload} className="mt-2 ml-12 max-h-48" />
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

// ── SubjectChainPane ──────────────────────────────────────────────────────

export function SubjectChainPane() {
  const dispatch = useDispatch<InspectorMachineEvent>();

  const subjectType = useSelector<InspectorState, string | null>((s) => s.subjectType);
  const subjectId = useSelector<InspectorState, string | null>((s) => s.subjectId);
  const subjectMode = useSelector<InspectorState, SubjectChainMode>((s) => s.subjectMode);
  const subjectChain = useSelector<InspectorState, SubjectChainEvent[]>((s) => s.subjectChain);
  const loading = useSelector<InspectorState, boolean>((s) => s.subjectChainLoading);
  const hasMore = useSelector<InspectorState, boolean>((s) => s.subjectChainHasMore);
  const depthCapped = useSelector<InspectorState, boolean>((s) => s.subjectDepthCapped);
  const expandedEffects = useSelector<InspectorState, Record<string, InspectorEffect[]>>((s) => s.expandedEffects);
  const loadingEffectsIds = useSelector<InspectorState, string[]>((s) => s.loadingEffects);

  if (!subjectType || !subjectId) {
    return (
      <div className="flex items-center justify-center h-full text-xs text-muted-foreground/50 tracking-wide">
        Select an entity to view its subject chain
      </div>
    );
  }

  const modes: { label: string; value: SubjectChainMode }[] = [
    { label: "Stream", value: "stream" },
    { label: "Descendants", value: "descendants" },
    { label: "Both", value: "both" },
  ];

  const shortId = subjectId.length > 8 ? subjectId.slice(0, 8) + "…" : subjectId;

  return (
    <div className="h-full flex flex-col">
      {/* Header */}
      <div className="px-3 py-2 border-b border-border shrink-0 space-y-1.5">
        <div className="flex items-center gap-2 min-w-0">
          <span className="text-[10px] font-mono text-muted-foreground/60 shrink-0">{subjectType}</span>
          <span className="text-[10px] font-mono text-foreground/80 truncate" title={subjectId}>{shortId}</span>
        </div>

        {/* Mode toggle */}
        <div className="flex items-center gap-1">
          {modes.map(({ label, value }) => (
            <button
              key={value}
              onClick={() => dispatch({ type: "ui/subject_mode_changed", payload: { mode: value } })}
              className={`px-2 py-0.5 rounded text-[10px] transition-all ${
                subjectMode === value
                  ? "bg-indigo-500/20 text-indigo-300 border border-indigo-500/30"
                  : "text-muted-foreground/60 hover:text-foreground border border-transparent hover:border-border"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      {/* Depth cap warning */}
      {depthCapped && (
        <div className="flex items-center gap-2 px-3 py-1.5 bg-yellow-500/8 border-b border-yellow-500/15 text-[10px] text-yellow-400/80 shrink-0">
          <AlertTriangle size={10} className="shrink-0" />
          Descendants truncated at depth 10. Some events may not be shown.
        </div>
      )}

      {/* Event list */}
      <div className="flex-1 overflow-y-auto">
        {loading && subjectChain.length === 0 ? (
          <div className="p-3 space-y-2 animate-pulse">
            {[...Array(5)].map((_, i) => (
              <div key={i} className="flex items-center gap-2">
                <div className="h-3 w-10 bg-muted rounded" />
                <div className="h-3 w-24 bg-muted rounded" />
                <div className="h-3 w-20 bg-muted rounded" />
                <div className="h-3 w-36 bg-muted rounded" />
              </div>
            ))}
          </div>
        ) : subjectChain.length === 0 ? (
          <div className="flex items-center justify-center h-32 text-xs text-muted-foreground/50">
            No events found
          </div>
        ) : (
          <>
            {subjectChain.map((event) => (
              <SubjectChainEventRow
                key={event.seq}
                event={event}
                showSourceBadge={subjectMode === "both"}
                effects={event.id ? expandedEffects[event.id] : undefined}
                loadingEffects={event.id ? loadingEffectsIds.includes(event.id) : false}
                dispatch={dispatch}
              />
            ))}

            {hasMore && (
              <div className="flex items-center justify-center py-3">
                <button
                  onClick={() => dispatch({ type: "ui/subject_chain_load_more" })}
                  disabled={loading}
                  className="text-[10px] text-muted-foreground/60 hover:text-foreground px-3 py-1.5 rounded border border-border hover:border-foreground/20 transition-all disabled:opacity-40"
                >
                  {loading ? "Loading…" : "Load more"}
                </button>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
