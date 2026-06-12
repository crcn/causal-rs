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
  workflowId: string | null,
  handler: string | null,
) {
  // Workflow changed → reset flow state
  if (workflowId !== draft.flowWorkflowId) {
    if (workflowId) {
      draft.flowWorkflowId = workflowId;
      draft.flowData = [];
      draft.flowSelection = null;
      draft.scrubberStart = null;
      draft.scrubberEnd = null;
      draft.scrubberPlaying = false;
      draft.logsFilter = {
        scope: "workflow",
        reactorId: null,
        workflowId,
      };
    } else {
      draft.flowWorkflowId = null;
      draft.flowData = [];
      draft.flowSelection = null;
      draft.scrubberStart = null;
      draft.scrubberEnd = null;
      draft.scrubberPlaying = false;
      draft.logsFilter = {
        scope: "reactor",
        reactorId: null,
        workflowId: null,
      };
    }
  }

  // Handler changed → update logs filter
  if (handler && handler !== draft.logsFilter.reactorId) {
    draft.logsFilter = {
      scope: "reactor",
      reactorId: handler,
      workflowId: draft.flowWorkflowId,
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
        if (draft.filters.workflowId && e.workflowId !== draft.filters.workflowId) {
          return false;
        }
        if (draft.filters.search) {
          const s = draft.filters.search.toLowerCase();
          const matches =
            e.name.toLowerCase().includes(s) ||
            e.payload.toLowerCase().includes(s) ||
            (e.workflowId ?? "").toLowerCase().includes(s);
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
      const { workflowId, descriptions } = event.payload;
      draft.descriptions[workflowId] = descriptions;
      break;
    }
    case "events/description_snapshots_loaded": {
      const { workflowId, snapshots } = event.payload;
      draft.descriptionSnapshots[workflowId] = snapshots;
      break;
    }
    case "events/aggregate_timeline_loaded": {
      const { workflowId, entries } = event.payload;
      draft.aggregateTimeline[workflowId] = entries;
      break;
    }
    case "events/outcomes_loaded": {
      const { workflowId, outcomes } = event.payload;
      draft.outcomes[workflowId] = outcomes;
      break;
    }
    case "events/attempts_loaded": {
      const { workflowId, attempts } = event.payload;
      draft.attempts[workflowId] = attempts;
      break;
    }
    case "events/workflows_loaded": {
      const { workflows, hasMore, append } = event.payload;
      if (append) {
        draft.workflows.push(...workflows);
      } else {
        draft.workflows = workflows;
      }
      draft.workflowsHasMore = hasMore;
      draft.workflowsLoading = false;
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
      applyNavigation(draft, event.payload.workflowId, null);
      break;
    case "ui/flow_closed":
      applyNavigation(draft, null, null);
      break;
    case "ui/handler_selected":
      applyNavigation(draft, draft.flowWorkflowId, event.payload.reactorId);
      break;
    case "location/changed":
      applyNavigation(draft, event.payload.workflowId, event.payload.handler);
      draft.filters.workflowId = event.payload.workflowId;
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
          scope: "workflow",
          reactorId: null,
          workflowId: draft.flowWorkflowId,
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
    case "ui/workflows_requested":
      draft.workflowsLoading = true;
      break;
    case "ui/load_more_workflows_requested":
      draft.workflowsLoading = true;
      break;
  }
};
