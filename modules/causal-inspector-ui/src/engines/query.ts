import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";
import type {
  InspectorEventsPage,
  InspectorCausalTree,
  InspectorCausalFlow,
  CorrelationSummaryPage,
  ReactorDependency,
  AggregateLifecycleEntry,
  ReactorLog,
  ReactorDescription,
  ReactorDescriptionSnapshot,
  AggregateTimelineEntry,
  ReactorOutcome,
} from "../types";
import {
  INSPECTOR_EVENTS,
  INSPECTOR_CAUSAL_TREE,
  INSPECTOR_CAUSAL_FLOW,
  INSPECTOR_CORRELATIONS,
  INSPECTOR_REACTOR_DEPENDENCIES,
  INSPECTOR_AGGREGATE_KEYS,
  INSPECTOR_AGGREGATE_LIFECYCLE,
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
 * Query engine — fetches data in response to state transitions.
 *
 * State-reactive: watches (curr, prev) diffs for navigation state.
 * Event-reactive: handles explicit requests (load_more, filter_changed, etc.).
 */
export const createQueryEngine = (
  transport: QueryTransport
): EngineCreator<InspectorState, InspectorMachineEvent> => {
  return (dispatch, getState) => {
    let flowPollTimer: ReturnType<typeof setInterval> | null = null;
    let correlationPollTimer: ReturnType<typeof setInterval> | null = null;
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
            correlationId: state.filters.correlationId || undefined,
            aggregateKey: state.filters.aggregateKey || undefined,
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

    const fetchLogs = async (correlationId: string) => {
      try {
        const data = await transport.query<{
          inspectorReactorLogsByCorrelation: ReactorLog[];
        }>(INSPECTOR_REACTOR_LOGS_BY_CORRELATION, { correlationId });
        dispatch({ type: "events/logs_loaded", payload: data.inspectorReactorLogsByCorrelation });
      } catch (e) {
        console.error("[causal-inspector] fetch logs failed:", e);
      }
    };

    let correlationCursor: string | null = null;

    const fetchCorrelations = async (opts?: { search?: string; append?: boolean }) => {
      const append = opts?.append ?? false;
      const cursor = append ? correlationCursor : undefined;

      try {
        const data = await transport.query<{
          inspectorCorrelations: CorrelationSummaryPage;
        }>(INSPECTOR_CORRELATIONS, {
          search: opts?.search || undefined,
          limit: 50,
          cursor: cursor || undefined,
        });

        correlationCursor = data.inspectorCorrelations.nextCursor;

        dispatch({
          type: "events/correlations_loaded",
          payload: {
            correlations: data.inspectorCorrelations.correlations,
            hasMore: data.inspectorCorrelations.nextCursor != null,
            append,
          },
        });
      } catch (e) {
        console.error("[causal-inspector] fetch correlations failed:", e);
      }
    };

    const fetchReactorDependencies = async () => {
      try {
        const data = await transport.query<{
          inspectorReactorDependencies: ReactorDependency[];
        }>(INSPECTOR_REACTOR_DEPENDENCIES);
        dispatch({
          type: "events/reactor_dependencies_loaded",
          payload: data.inspectorReactorDependencies,
        });
      } catch (e) {
        console.error("[causal-inspector] fetch reactor dependencies failed:", e);
      }
    };

    const fetchAggregateKeys = async () => {
      try {
        const data = await transport.query<{
          inspectorAggregateKeys: string[];
        }>(INSPECTOR_AGGREGATE_KEYS);
        dispatch({
          type: "events/aggregate_keys_loaded",
          payload: data.inspectorAggregateKeys,
        });
      } catch (e) {
        console.error("[causal-inspector] fetch aggregate keys failed:", e);
      }
    };

    const fetchAggregateLifecycle = async (aggregateKey: string) => {
      try {
        const data = await transport.query<{
          inspectorAggregateLifecycle: AggregateLifecycleEntry[];
        }>(INSPECTOR_AGGREGATE_LIFECYCLE, { aggregateKey, limit: 200 });
        dispatch({
          type: "events/aggregate_lifecycle_loaded",
          payload: { key: aggregateKey, entries: data.inspectorAggregateLifecycle },
        });
      } catch (e) {
        console.error("[causal-inspector] fetch aggregate lifecycle failed:", e);
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

    const stopCorrelationPolling = () => {
      if (correlationPollTimer) {
        clearInterval(correlationPollTimer);
        correlationPollTimer = null;
      }
    };

    // Initial load
    fetchEvents();
    fetchCorrelations();
    fetchReactorDependencies();
    fetchAggregateKeys();

    return {
      handleEvent: (event, curr, prev) => {
        // ── State-reactive: navigation transitions ──

        if (curr.flowCorrelationId !== prev.flowCorrelationId) {
          if (curr.flowCorrelationId) {
            // Flow opened
            fetchFlow(curr.flowCorrelationId);
            startFlowPolling(curr.flowCorrelationId);
            fetchLogs(curr.flowCorrelationId);
            stopCorrelationPolling();
          } else {
            // Flow closed
            stopFlowPolling();
          }
        }

        // ── Event-reactive: explicit user requests ──

        switch (event.type) {
          case "ui/load_more_requested":
            fetchEvents();
            break;

          case "ui/event_selected":
            fetchCausalTree(event.payload.seq);
            break;

          case "ui/filter_changed":
            fetchEvents();
            break;

          case "ui/load_more_correlations_requested":
            fetchCorrelations({ append: true });
            break;

          case "ui/correlations_requested":
            correlationCursor = null;
            fetchCorrelations({ search: event.payload.search });
            stopCorrelationPolling();
            correlationPollTimer = setInterval(() => fetchCorrelations({ search: event.payload.search }), 5000);
            break;

          case "ui/aggregate_lifecycle_requested":
            fetchAggregateLifecycle(event.payload.aggregateKey);
            break;
        }
      },
      dispose: () => {
        stopFlowPolling();
        stopCorrelationPolling();
      },
    };
  };
};
