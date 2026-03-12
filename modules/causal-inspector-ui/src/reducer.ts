import type { Draft } from "immer";
import type { Reducer } from "./machine";
import type { InspectorMachineEvent } from "./events";
import type { InspectorState } from "./state";

/**
 * Shared navigation logic used by both user-initiated facts
 * (ui/flow_opened, ui/handler_selected) and browser-initiated
 * navigation (location/changed from popstate).
 */
function applyNavigation(
  draft: Draft<InspectorState>,
  correlationId: string | null,
  handler: string | null,
) {
  // Correlation changed → reset flow state
  if (correlationId !== draft.flowCorrelationId) {
    if (correlationId) {
      draft.flowCorrelationId = correlationId;
      draft.flowData = [];
      draft.flowSelection = null;
      draft.scrubberStart = null;
      draft.scrubberEnd = null;
      draft.scrubberPlaying = false;
      draft.logsFilter = {
        scope: "correlation",
        reactorId: null,
        correlationId,
      };
    } else {
      draft.flowCorrelationId = null;
      draft.flowData = [];
      draft.flowSelection = null;
      draft.scrubberStart = null;
      draft.scrubberEnd = null;
      draft.scrubberPlaying = false;
      draft.logsFilter = {
        scope: "reactor",
        reactorId: null,
        correlationId: null,
      };
    }
  }

  // Handler changed → update logs filter
  if (handler && handler !== draft.logsFilter.reactorId) {
    draft.logsFilter = {
      scope: "reactor",
      reactorId: handler,
      correlationId: draft.flowCorrelationId,
    };
  }
}

export const reducer: Reducer<InspectorState, InspectorMachineEvent> = (
  draft: Draft<InspectorState>,
  event: InspectorMachineEvent
) => {
  switch (event.type) {
    // ── Subscription ──

    case "events/received": {
      const newEvents = event.payload;
      const filtered = newEvents.filter((e) => {
        if (draft.filters.correlationId && e.correlationId !== draft.filters.correlationId) {
          return false;
        }
        if (draft.filters.search) {
          const s = draft.filters.search.toLowerCase();
          const matches =
            e.name.toLowerCase().includes(s) ||
            e.payload.toLowerCase().includes(s) ||
            (e.correlationId ?? "").toLowerCase().includes(s);
          if (!matches) return false;
        }
        return true;
      });
      if (filtered.length > 0) {
        draft.events.unshift(...filtered);
      }
      break;
    }
    case "events/subscription_connected":
      draft.subscription = "connected";
      break;
    case "events/subscription_error":
      draft.subscription = "error";
      break;

    // ── Query results ──

    case "events/page_loaded": {
      const { events, hasMore } = event.payload;
      draft.events.push(...events);
      draft.hasMore = hasMore;
      draft.loading = false;
      break;
    }
    case "events/causal_tree_loaded":
      draft.causalTree = event.payload;
      break;
    case "events/flow_loaded":
      draft.flowData = event.payload;
      break;
    case "events/logs_loaded":
      draft.logs = event.payload;
      break;
    case "events/descriptions_loaded": {
      const { correlationId, descriptions } = event.payload;
      draft.descriptions[correlationId] = descriptions;
      break;
    }
    case "events/description_snapshots_loaded": {
      const { correlationId, snapshots } = event.payload;
      draft.descriptionSnapshots[correlationId] = snapshots;
      break;
    }
    case "events/aggregate_timeline_loaded": {
      const { correlationId, entries } = event.payload;
      draft.aggregateTimeline[correlationId] = entries;
      break;
    }
    case "events/outcomes_loaded": {
      const { correlationId, outcomes } = event.payload;
      draft.outcomes[correlationId] = outcomes;
      break;
    }
    case "events/correlations_loaded": {
      const { correlations, hasMore, append } = event.payload;
      if (append) {
        draft.correlations.push(...correlations);
      } else {
        draft.correlations = correlations;
      }
      draft.correlationsHasMore = hasMore;
      draft.correlationsLoading = false;
      break;
    }
    case "events/reactor_dependencies_loaded":
      draft.reactorDependencies = event.payload;
      break;
    case "events/aggregate_keys_loaded":
      draft.aggregateKeys = event.payload;
      break;
    case "events/aggregate_lifecycle_loaded":
      draft.aggregateLifecycleKey = event.payload.key;
      draft.aggregateLifecycle = event.payload.entries;
      break;

    // ── Navigation (user facts + browser popstate) ──

    case "ui/flow_opened":
      applyNavigation(draft, event.payload.correlationId, null);
      break;
    case "ui/flow_closed":
      applyNavigation(draft, null, null);
      break;
    case "ui/handler_selected":
      applyNavigation(draft, draft.flowCorrelationId, event.payload.reactorId);
      break;
    case "location/changed":
      applyNavigation(draft, event.payload.correlationId, event.payload.handler);
      break;

    // ── UI ──

    case "ui/event_selected":
      draft.selectedSeq = event.payload.seq;
      break;
    case "ui/event_deselected":
      draft.selectedSeq = null;
      draft.causalTree = null;
      break;
    case "ui/flow_node_selected":
      draft.flowSelection = event.payload;
      // Clear reactor filter when deselecting a node
      if (event.payload == null && draft.logsFilter.reactorId != null) {
        draft.logsFilter = {
          scope: "correlation",
          reactorId: null,
          correlationId: draft.flowCorrelationId,
        };
      }
      break;
    case "ui/filter_changed":
      Object.assign(draft.filters, event.payload);
      break;
    case "ui/load_more_requested":
      draft.loading = true;
      break;
    case "ui/layout_changed":
      draft.paneLayout = event.payload;
      break;
    case "ui/scrubber_start_changed":
      draft.scrubberStart = event.payload.start;
      break;
    case "ui/scrubber_end_changed":
      draft.scrubberEnd = event.payload.end;
      break;
    case "ui/scrubber_play_toggled":
      draft.scrubberPlaying = !draft.scrubberPlaying;
      break;
    case "ui/scrubber_speed_changed":
      draft.scrubberSpeed = event.payload.speed;
      break;
    case "ui/correlations_requested":
      draft.correlationsLoading = true;
      break;
    case "ui/load_more_correlations_requested":
      draft.correlationsLoading = true;
      break;
  }
};
