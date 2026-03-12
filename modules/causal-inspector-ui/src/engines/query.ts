import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";
import type {
  InspectorEventsPage,
  InspectorCausalTree,
  InspectorCausalFlow,
  ReactorLog,
  ReactorDescription,
  ReactorDescriptionSnapshot,
  AggregateTimelineEntry,
  ReactorOutcome,
  LogsFilter,
} from "../types";
import {
  INSPECTOR_EVENTS,
  INSPECTOR_CAUSAL_TREE,
  INSPECTOR_CAUSAL_FLOW,
  INSPECTOR_REACTOR_LOGS,
  INSPECTOR_REACTOR_LOGS_BY_CORRELATION,
  INSPECTOR_REACTOR_DESCRIPTIONS,
  INSPECTOR_REACTOR_DESCRIPTION_SNAPSHOTS,
  INSPECTOR_AGGREGATE_TIMELINE,
  INSPECTOR_REACTOR_OUTCOMES,
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
): EngineCreator<InspectorState, InspectorMachineEvent> => {
  return (dispatch, getState) => {
    let flowPollTimer: ReturnType<typeof setInterval> | null = null;
    // Stale-response guards
    let activeCausalSeq: number | null = null;
    let activeFlowCorrelationId: string | null = null;

    const fetchEvents = async () => {
      const state = getState();
      const cursor =
        state.events.length > 0
          ? state.events[state.events.length - 1].seq
          : undefined;

      try {
        const data = await transport.query<{ inspectorEvents: InspectorEventsPage }>(
          INSPECTOR_EVENTS,
          {
            limit: 50,
            cursor,
            search: state.filters.search || undefined,
            from: state.filters.from || undefined,
            to: state.filters.to || undefined,
            correlationId: state.filters.correlationId || undefined,
          }
        );

        dispatch({
          type: "events/page_loaded",
          payload: {
            events: data.inspectorEvents.events,
            hasMore: data.inspectorEvents.nextCursor != null,
          },
        });
      } catch (e) {
        console.error("[causal-inspector] fetch events failed:", e);
      }
    };

    const fetchCausalTree = async (seq: number) => {
      activeCausalSeq = seq;
      try {
        const data = await transport.query<{
          inspectorCausalTree: InspectorCausalTree;
        }>(INSPECTOR_CAUSAL_TREE, { seq });
        if (activeCausalSeq !== seq) return; // stale
        dispatch({
          type: "events/causal_tree_loaded",
          payload: data.inspectorCausalTree,
        });
      } catch (e) {
        console.error("[causal-inspector] fetch causal tree failed:", e);
      }
    };

    const fetchFlow = async (correlationId: string) => {
      activeFlowCorrelationId = correlationId;
      try {
        const data = await transport.query<{
          inspectorCausalFlow: InspectorCausalFlow;
        }>(INSPECTOR_CAUSAL_FLOW, { correlationId });
        if (activeFlowCorrelationId !== correlationId) return; // stale
        dispatch({
          type: "events/flow_loaded",
          payload: data.inspectorCausalFlow.events,
        });
      } catch (e) {
        console.error("[causal-inspector] fetch flow failed:", e);
      }
    };

    const fetchFlowMetadata = async (correlationId: string) => {
      try {
        const [descData, snapshotData, aggTimelineData, outcomeData] = await Promise.all([
          transport.query<{
            inspectorReactorDescriptions: ReactorDescription[];
          }>(INSPECTOR_REACTOR_DESCRIPTIONS, { correlationId }),
          transport.query<{
            inspectorReactorDescriptionSnapshots: ReactorDescriptionSnapshot[];
          }>(INSPECTOR_REACTOR_DESCRIPTION_SNAPSHOTS, { correlationId }),
          transport.query<{
            inspectorAggregateTimeline: AggregateTimelineEntry[];
          }>(INSPECTOR_AGGREGATE_TIMELINE, { correlationId }),
          transport.query<{
            inspectorReactorOutcomes: ReactorOutcome[];
          }>(INSPECTOR_REACTOR_OUTCOMES, { correlationId }),
        ]);
        if (activeFlowCorrelationId !== correlationId) return; // stale

        dispatch({
          type: "events/descriptions_loaded",
          payload: {
            correlationId,
            descriptions: descData.inspectorReactorDescriptions,
          },
        });
        dispatch({
          type: "events/description_snapshots_loaded",
          payload: {
            correlationId,
            snapshots: snapshotData.inspectorReactorDescriptionSnapshots,
          },
        });
        dispatch({
          type: "events/aggregate_timeline_loaded",
          payload: {
            correlationId,
            entries: aggTimelineData.inspectorAggregateTimeline,
          },
        });
        dispatch({
          type: "events/outcomes_loaded",
          payload: {
            correlationId,
            outcomes: outcomeData.inspectorReactorOutcomes,
          },
        });
      } catch (e) {
        console.error("[causal-inspector] fetch flow metadata failed:", e);
      }
    };

    const fetchLogs = async (filter: LogsFilter) => {
      try {
        if (filter.scope === "correlation" && filter.correlationId) {
          const data = await transport.query<{
            inspectorReactorLogsByCorrelation: ReactorLog[];
          }>(INSPECTOR_REACTOR_LOGS_BY_CORRELATION, { correlationId: filter.correlationId });
          dispatch({ type: "events/logs_loaded", payload: data.inspectorReactorLogsByCorrelation });
        } else if (filter.eventId && filter.reactorId) {
          const data = await transport.query<{
            inspectorReactorLogs: ReactorLog[];
          }>(INSPECTOR_REACTOR_LOGS, { eventId: filter.eventId, reactorId: filter.reactorId });
          dispatch({ type: "events/logs_loaded", payload: data.inspectorReactorLogs });
        }
      } catch (e) {
        console.error("[causal-inspector] fetch logs failed:", e);
      }
    };

    const startFlowPolling = (correlationId: string) => {
      stopFlowPolling();
      fetchFlowMetadata(correlationId);
      flowPollTimer = setInterval(() => fetchFlowMetadata(correlationId), 5000);
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
            const correlationId = event.payload.correlationId;
            fetchFlow(correlationId);
            startFlowPolling(correlationId);
            fetchLogs(curr.logsFilter);
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
