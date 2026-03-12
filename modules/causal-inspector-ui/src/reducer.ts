import type { Draft } from "immer";
import type { Reducer } from "./machine";
import type { InspectorMachineEvent } from "./events";
import type { InspectorState } from "./state";

export const reducer: Reducer<InspectorState, InspectorMachineEvent> = (
  draft: Draft<InspectorState>,
  event: InspectorMachineEvent
) => {
  switch (event.type) {
    // ── Subscription ──

    case "events/received": {
      const newEvents = event.payload;
      // Filter subscription events against active filters so they don't
      // pollute the view when the user has a search or correlation filter.
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
    case "events/correlations_loaded":
      draft.correlations = event.payload;
      draft.correlationsLoading = false;
      break;

    // ── UI ──

    case "ui/event_selected":
      draft.selectedSeq = event.payload.seq;
      break;
    case "ui/event_deselected":
      draft.selectedSeq = null;
      draft.causalTree = null;
      break;
    case "ui/flow_opened":
      draft.flowCorrelationId = event.payload.correlationId;
      draft.flowData = [];
      draft.flowSelection = null;
      draft.scrubberPosition = null;
      draft.scrubberPlaying = false;
      // Auto-show logs for this correlation
      draft.logsFilter = {
        scope: "correlation",
        eventId: null,
        reactorId: null,
        correlationId: event.payload.correlationId,
      };
      break;
    case "ui/flow_closed":
      draft.flowCorrelationId = null;
      draft.flowData = [];
      draft.flowSelection = null;
      draft.scrubberPosition = null;
      draft.scrubberPlaying = false;
      break;
    case "ui/flow_node_selected":
      draft.flowSelection = event.payload;
      break;
    case "ui/filter_changed":
      Object.assign(draft.filters, event.payload);
      break;
    case "ui/logs_filter_changed":
      Object.assign(draft.logsFilter, event.payload);
      break;
    case "ui/load_more_requested":
      draft.loading = true;
      break;
    case "ui/layout_changed":
      draft.paneLayout = event.payload;
      break;
    case "ui/scrubber_moved":
      draft.scrubberPosition = event.payload.position;
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
  }
};
