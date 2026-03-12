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
      draft.events.unshift(...newEvents);
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
    case "events/outcomes_loaded": {
      const { correlationId, outcomes } = event.payload;
      draft.outcomes[correlationId] = outcomes;
      break;
    }

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
      break;
    case "ui/flow_closed":
      draft.flowCorrelationId = null;
      draft.flowData = [];
      draft.flowSelection = null;
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
  }
};
