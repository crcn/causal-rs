import type { EngineCreator } from "../machine";
import type { AdminMachineEvent } from "../events";
import type { AdminState } from "../state";
import type {
  AdminEventsPage,
  AdminCausalTree,
  AdminCausalFlow,
  HandlerLog,
  HandlerDescription,
  HandlerOutcome,
  LogsFilter,
} from "../types";
import {
  ADMIN_EVENTS,
  ADMIN_CAUSAL_TREE,
  ADMIN_CAUSAL_FLOW,
  ADMIN_HANDLER_LOGS,
  ADMIN_HANDLER_LOGS_BY_RUN,
  ADMIN_HANDLER_DESCRIPTIONS,
  ADMIN_HANDLER_OUTCOMES,
} from "../queries";

export type QueryTransport = {
  /** Execute a GraphQL query. Returns the `data` object. */
  query: <T = unknown>(
    query: string,
    variables?: Record<string, unknown>
  ) => Promise<T>;
};

/**
 * Query engine — fetches data on demand in response to UI events.
 *
 * Reacts to:
 * - `ui/load_more_requested` → fetch next page of events
 * - `ui/event_selected` → fetch causal tree
 * - `ui/flow_opened` → fetch flow data + start polling descriptions/outcomes
 * - `ui/flow_closed` → stop polling
 * - `ui/filter_changed` → refetch events
 */
export const createQueryEngine = (
  transport: QueryTransport
): EngineCreator<AdminState, AdminMachineEvent> => {
  return (dispatch, getState) => {
    let flowPollTimer: ReturnType<typeof setInterval> | null = null;
    // Stale-response guards: track the latest request target so late-arriving
    // responses for a previous selection are discarded.
    let activeCausalSeq: number | null = null;
    let activeFlowRunId: string | null = null;

    const fetchEvents = async () => {
      const state = getState();
      const cursor =
        state.events.length > 0
          ? state.events[state.events.length - 1].seq
          : undefined;

      try {
        const data = await transport.query<{ adminEvents: AdminEventsPage }>(
          ADMIN_EVENTS,
          {
            limit: 50,
            cursor,
            search: state.filters.search || undefined,
            from: state.filters.from || undefined,
            to: state.filters.to || undefined,
            runId: state.filters.runId || undefined,
          }
        );

        dispatch({
          type: "events/page_loaded",
          payload: {
            events: data.adminEvents.events,
            hasMore: data.adminEvents.nextCursor != null,
          },
        });
      } catch (e) {
        console.error("[causal-admin] fetch events failed:", e);
      }
    };

    const fetchCausalTree = async (seq: number) => {
      activeCausalSeq = seq;
      try {
        const data = await transport.query<{
          adminCausalTree: AdminCausalTree;
        }>(ADMIN_CAUSAL_TREE, { seq });
        if (activeCausalSeq !== seq) return; // stale
        dispatch({
          type: "events/causal_tree_loaded",
          payload: data.adminCausalTree,
        });
      } catch (e) {
        console.error("[causal-admin] fetch causal tree failed:", e);
      }
    };

    const fetchFlow = async (runId: string) => {
      activeFlowRunId = runId;
      try {
        const data = await transport.query<{
          adminCausalFlow: AdminCausalFlow;
        }>(ADMIN_CAUSAL_FLOW, { runId });
        if (activeFlowRunId !== runId) return; // stale
        dispatch({
          type: "events/flow_loaded",
          payload: data.adminCausalFlow.events,
        });
      } catch (e) {
        console.error("[causal-admin] fetch flow failed:", e);
      }
    };

    const fetchFlowMetadata = async (runId: string) => {
      try {
        const [descData, outcomeData] = await Promise.all([
          transport.query<{
            adminHandlerDescriptions: HandlerDescription[];
          }>(ADMIN_HANDLER_DESCRIPTIONS, { runId }),
          transport.query<{
            adminHandlerOutcomes: HandlerOutcome[];
          }>(ADMIN_HANDLER_OUTCOMES, { runId }),
        ]);
        if (activeFlowRunId !== runId) return; // stale

        dispatch({
          type: "events/descriptions_loaded",
          payload: {
            runId,
            descriptions: descData.adminHandlerDescriptions,
          },
        });
        dispatch({
          type: "events/outcomes_loaded",
          payload: {
            runId,
            outcomes: outcomeData.adminHandlerOutcomes,
          },
        });
      } catch (e) {
        console.error("[causal-admin] fetch flow metadata failed:", e);
      }
    };

    const fetchLogs = async (filter: LogsFilter) => {
      try {
        if (filter.scope === "run" && filter.runId) {
          const data = await transport.query<{
            adminHandlerLogsByRun: HandlerLog[];
          }>(ADMIN_HANDLER_LOGS_BY_RUN, { runId: filter.runId });
          dispatch({ type: "events/logs_loaded", payload: data.adminHandlerLogsByRun });
        } else if (filter.eventId && filter.handlerId) {
          const data = await transport.query<{
            adminHandlerLogs: HandlerLog[];
          }>(ADMIN_HANDLER_LOGS, { eventId: filter.eventId, handlerId: filter.handlerId });
          dispatch({ type: "events/logs_loaded", payload: data.adminHandlerLogs });
        }
      } catch (e) {
        console.error("[causal-admin] fetch logs failed:", e);
      }
    };

    const startFlowPolling = (runId: string) => {
      stopFlowPolling();
      // Fetch immediately then poll every 5s
      fetchFlowMetadata(runId);
      flowPollTimer = setInterval(() => fetchFlowMetadata(runId), 5000);
    };

    const stopFlowPolling = () => {
      if (flowPollTimer) {
        clearInterval(flowPollTimer);
        flowPollTimer = null;
      }
    };

    // Initial load
    fetchEvents();

    return {
      handleEvent: (event, curr, prev) => {
        switch (event.type) {
          case "ui/load_more_requested":
            fetchEvents();
            break;

          case "ui/event_selected":
            fetchCausalTree(event.payload.seq);
            break;

          case "ui/flow_opened": {
            const runId = event.payload.runId;
            fetchFlow(runId);
            startFlowPolling(runId);
            break;
          }

          case "ui/flow_closed":
            stopFlowPolling();
            break;

          case "ui/filter_changed":
            // Clear existing events and refetch
            dispatch({
              type: "events/page_loaded",
              payload: { events: [], hasMore: true },
            });
            fetchEvents();
            break;

          case "ui/logs_filter_changed":
            fetchLogs(curr.logsFilter);
            break;
        }
      },
      dispose: () => {
        stopFlowPolling();
      },
    };
  };
};
