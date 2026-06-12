import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";
import type {
  InspectorEventsPage,
  InspectorCausalTree,
  InspectorCausalFlow,
  WorkflowSummaryPage,
  ReactorDependency,
  AggregateLifecycleEntry,
  ReactorLog,
  ReactorDescription,
  ReactorDescriptionSnapshot,
  AggregateTimelineEntry,
  ReactorOutcome,
  ReactorAttempt,
} from "../types";
import {
  INSPECTOR_EVENTS,
  INSPECTOR_CAUSAL_TREE,
  INSPECTOR_CAUSAL_FLOW,
  INSPECTOR_WORKFLOWS,
  INSPECTOR_REACTOR_DEPENDENCIES,
  INSPECTOR_AGGREGATE_KEYS,
  INSPECTOR_AGGREGATE_LIFECYCLE,
  INSPECTOR_REACTOR_LOGS_BY_WORKFLOW,
  INSPECTOR_REACTOR_DESCRIPTIONS,
  INSPECTOR_REACTOR_DESCRIPTION_SNAPSHOTS,
  INSPECTOR_AGGREGATE_TIMELINE,
  INSPECTOR_REACTOR_OUTCOMES,
  INSPECTOR_REACTOR_ATTEMPTS,
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
    let workflowPollTimer: ReturnType<typeof setInterval> | null = null;
    // Stale-response guards
    let activeCausalSeq: number | null = null;
    let activeFlowWorkflowId: string | null = null;

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
            workflowId: state.filters.workflowId || undefined,
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

    const fetchFlow = async (workflowId: string) => {
      activeFlowWorkflowId = workflowId;
      try {
        const data = await transport.query<{
          inspectorCausalFlow: InspectorCausalFlow;
        }>(INSPECTOR_CAUSAL_FLOW, { workflowId });
        if (activeFlowWorkflowId !== workflowId) return; // stale
        dispatch({
          type: "events/flow_loaded",
          payload: data.inspectorCausalFlow.events,
        });
      } catch (e) {
        console.error("[causal-inspector] fetch flow failed:", e);
      }
    };

    const fetchFlowMetadata = async (workflowId: string) => {
      try {
        const [descData, snapshotData, aggTimelineData, outcomeData, attemptData] = await Promise.all([
          transport.query<{
            inspectorReactorDescriptions: ReactorDescription[];
          }>(INSPECTOR_REACTOR_DESCRIPTIONS, { workflowId }),
          transport.query<{
            inspectorReactorDescriptionSnapshots: ReactorDescriptionSnapshot[];
          }>(INSPECTOR_REACTOR_DESCRIPTION_SNAPSHOTS, { workflowId }),
          transport.query<{
            inspectorAggregateTimeline: AggregateTimelineEntry[];
          }>(INSPECTOR_AGGREGATE_TIMELINE, { workflowId }),
          transport.query<{
            inspectorReactorOutcomes: ReactorOutcome[];
          }>(INSPECTOR_REACTOR_OUTCOMES, { workflowId }),
          transport.query<{
            inspectorReactorAttempts: ReactorAttempt[];
          }>(INSPECTOR_REACTOR_ATTEMPTS, { workflowId }),
        ]);
        if (activeFlowWorkflowId !== workflowId) return; // stale

        dispatch({
          type: "events/descriptions_loaded",
          payload: {
            workflowId,
            descriptions: descData.inspectorReactorDescriptions,
          },
        });
        dispatch({
          type: "events/description_snapshots_loaded",
          payload: {
            workflowId,
            snapshots: snapshotData.inspectorReactorDescriptionSnapshots,
          },
        });
        dispatch({
          type: "events/aggregate_timeline_loaded",
          payload: {
            workflowId,
            entries: aggTimelineData.inspectorAggregateTimeline,
          },
        });
        dispatch({
          type: "events/outcomes_loaded",
          payload: {
            workflowId,
            outcomes: outcomeData.inspectorReactorOutcomes,
          },
        });
        dispatch({
          type: "events/attempts_loaded",
          payload: {
            workflowId,
            attempts: attemptData.inspectorReactorAttempts,
          },
        });
      } catch (e) {
        console.error("[causal-inspector] fetch flow metadata failed:", e);
      }
    };

    const fetchLogs = async (workflowId: string) => {
      try {
        const data = await transport.query<{
          inspectorReactorLogsByWorkflow: ReactorLog[];
        }>(INSPECTOR_REACTOR_LOGS_BY_WORKFLOW, { workflowId });
        dispatch({ type: "events/logs_loaded", payload: data.inspectorReactorLogsByWorkflow });
      } catch (e) {
        console.error("[causal-inspector] fetch logs failed:", e);
      }
    };

    let workflowCursor: string | null = null;

    const fetchWorkflows = async (opts?: { search?: string; append?: boolean }) => {
      const append = opts?.append ?? false;
      const cursor = append ? workflowCursor : undefined;

      try {
        const data = await transport.query<{
          inspectorWorkflows: WorkflowSummaryPage;
        }>(INSPECTOR_WORKFLOWS, {
          search: opts?.search || undefined,
          limit: 50,
          cursor: cursor || undefined,
        });

        workflowCursor = data.inspectorWorkflows.nextCursor;

        dispatch({
          type: "events/workflows_loaded",
          payload: {
            workflows: data.inspectorWorkflows.workflows,
            hasMore: data.inspectorWorkflows.nextCursor != null,
            append,
          },
        });
      } catch (e) {
        console.error("[causal-inspector] fetch workflows failed:", e);
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

    const startFlowPolling = (workflowId: string) => {
      stopFlowPolling();
      fetchFlowMetadata(workflowId);
      flowPollTimer = setInterval(() => fetchFlowMetadata(workflowId), 5000);
    };

    const stopFlowPolling = () => {
      if (flowPollTimer) {
        clearInterval(flowPollTimer);
        flowPollTimer = null;
      }
    };

    const stopWorkflowPolling = () => {
      if (workflowPollTimer) {
        clearInterval(workflowPollTimer);
        workflowPollTimer = null;
      }
    };

    // Initial load
    fetchEvents();
    fetchWorkflows();
    fetchReactorDependencies();
    fetchAggregateKeys();

    return {
      handleEvent: (event, curr, prev) => {
        // ── State-reactive: navigation transitions ──

        if (curr.flowWorkflowId !== prev.flowWorkflowId) {
          if (curr.flowWorkflowId) {
            // Flow opened
            fetchFlow(curr.flowWorkflowId);
            startFlowPolling(curr.flowWorkflowId);
            fetchLogs(curr.flowWorkflowId);
            stopWorkflowPolling();
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

          case "ui/load_more_workflows_requested":
            fetchWorkflows({ append: true });
            break;

          case "ui/workflows_requested":
            workflowCursor = null;
            fetchWorkflows({ search: event.payload.search });
            stopWorkflowPolling();
            workflowPollTimer = setInterval(() => fetchWorkflows({ search: event.payload.search }), 5000);
            break;

          case "ui/aggregate_lifecycle_requested":
            fetchAggregateLifecycle(event.payload.aggregateKey);
            break;
        }
      },
      dispose: () => {
        stopFlowPolling();
        stopWorkflowPolling();
      },
    };
  };
};
