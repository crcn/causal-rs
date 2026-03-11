import type { Draft } from "immer";
import type { Reducer } from "./machine";
import type { AdminMachineEvent } from "./events";
import type { AdminState } from "./state";

export const reducer: Reducer<AdminState, AdminMachineEvent> = (
  draft: Draft<AdminState>,
  event: AdminMachineEvent
) => {
  switch (event.type) {
    // ── Subscription ──

    case "events/received": {
      const newEvents = event.payload!;
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
      const { events, hasMore } = event.payload!;
      draft.events.push(...events);
      draft.hasMore = hasMore;
      draft.loading = false;
      break;
    }
    case "events/causal_tree_loaded":
      draft.causalTree = event.payload!;
      break;
    case "events/flow_loaded":
      draft.flowData = event.payload!;
      break;
    case "events/logs_loaded":
      draft.logs = event.payload!;
      break;
    case "events/descriptions_loaded": {
      const { runId, descriptions } = event.payload!;
      draft.descriptions[runId] = descriptions;
      break;
    }
    case "events/outcomes_loaded": {
      const { runId, outcomes } = event.payload!;
      draft.outcomes[runId] = outcomes;
      break;
    }

    // ── UI ──

    case "ui/event_selected":
      draft.selectedSeq = event.payload!.seq;
      break;
    case "ui/event_deselected":
      draft.selectedSeq = null;
      draft.causalTree = null;
      break;
    case "ui/flow_opened":
      draft.flowRunId = event.payload!.runId;
      draft.flowData = [];
      draft.flowGraph = null;
      draft.flowSelection = { nodeId: null, eventSeq: null };
      draft.scrubberPosition = null;
      break;
    case "ui/flow_closed":
      draft.flowRunId = null;
      draft.flowData = [];
      draft.flowGraph = null;
      draft.flowSelection = { nodeId: null, eventSeq: null };
      draft.scrubberPosition = null;
      break;
    case "ui/flow_node_selected":
      draft.flowSelection = event.payload!;
      break;
    case "ui/filter_changed":
      Object.assign(draft.filters, event.payload);
      break;
    case "ui/logs_filter_changed":
      Object.assign(draft.logsFilter, event.payload);
      break;
    case "ui/scrubber_moved":
      draft.scrubberPosition = event.payload!.position;
      break;
    case "ui/load_more_requested":
      draft.loading = true;
      break;

    // ── Computed ──

    case "flow/graph_built":
      draft.flowGraph = event.payload!;
      break;
  }
};
