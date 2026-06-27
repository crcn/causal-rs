import type {
  InspectorEvent,
  WorkflowSummary,
  ReactorDependency,
  AggregateLifecycleEntry,
  FilterState,
  FlowSelection,
  ReactorDescription,
  ReactorDescriptionSnapshot,
  AggregateTimelineEntry,
  ReactorLog,
  ReactorOutcome,
  ReactorAttempt,
  LogsFilter,
  PaneLayout,
  SubjectChainEvent,
  SubjectChainMode,
  InspectorEffect,
} from "./types";

export type InspectorState = {
  // Event stream
  events: InspectorEvent[];
  hasMore: boolean;
  loading: boolean;

  // Selection
  selectedSeq: number | null;

  // Flow (causal DAG) — keyed by workflow_id
  flowWorkflowId: string | null;
  flowData: InspectorEvent[];
  flowSelection: FlowSelection;
  /** Workflow whose first errored reactor should be auto-selected once its
   *  outcomes load (set when a flow is opened via the Workflows error pill). */
  pendingErrorFocus: string | null;

  // Time scrubber — range window into events
  scrubberStart: number | null;   // null = beginning (no lower bound)
  scrubberEnd: number | null;     // null = show all (no upper bound)
  scrubberPlaying: boolean;
  scrubberSpeed: number;

  // Causal tree
  causalTree: { events: InspectorEvent[]; rootSeq: number } | null;

  // Filters
  filters: FilterState;

  // Logs
  logs: ReactorLog[];
  logsFilter: LogsFilter;

  // Reactor metadata (keyed by workflowId)
  descriptions: Record<string, ReactorDescription[]>;
  descriptionSnapshots: Record<string, ReactorDescriptionSnapshot[]>;
  aggregateTimeline: Record<string, AggregateTimelineEntry[]>;
  outcomes: Record<string, ReactorOutcome[]>;
  attempts: Record<string, ReactorAttempt[]>;

  // Workflows
  workflows: WorkflowSummary[];
  workflowsLoading: boolean;
  workflowsHasMore: boolean;

  // Reactor dependency map
  reactorDependencies: ReactorDependency[];

  // Aggregate lifecycle
  aggregateKeys: string[];
  aggregateLifecycle: AggregateLifecycleEntry[];
  aggregateLifecycleKey: string | null;

  // Subscription status
  subscription: "connected" | "disconnected" | "error";

  // Pane layout (opaque JSON — interpreted by the host app)
  paneLayout: PaneLayout | null;

  // Entity-scoped inspection
  subjectType: string | null;
  subjectId: string | null;
  subjectMode: SubjectChainMode;
  subjectChain: SubjectChainEvent[];
  subjectChainLoading: boolean;
  subjectChainHasMore: boolean;
  subjectChainCursor: number | null;
  subjectDepthCapped: boolean;
  expandedEffects: Record<string, InspectorEffect[]>;
  loadingEffects: string[];
};

export const initialState: InspectorState = {
  events: [],
  hasMore: true,
  loading: false,

  selectedSeq: null,

  flowWorkflowId: null,
  flowData: [],
  flowSelection: null,
  pendingErrorFocus: null,

  scrubberStart: null,
  scrubberEnd: null,
  scrubberPlaying: false,
  scrubberSpeed: 300,

  causalTree: null,

  filters: {
    search: "",
    workflowId: null,
    aggregateKey: null,
  },

  logs: [],
  logsFilter: {
    scope: "reactor",
    reactorId: null,
    workflowId: null,
  },

  descriptions: {},
  descriptionSnapshots: {},
  aggregateTimeline: {},
  outcomes: {},
  attempts: {},

  workflows: [],
  workflowsLoading: false,
  workflowsHasMore: true,

  reactorDependencies: [],

  aggregateKeys: [],
  aggregateLifecycle: [],
  aggregateLifecycleKey: null,

  subscription: "disconnected",

  paneLayout: null,

  subjectType: null,
  subjectId: null,
  subjectMode: "both",
  subjectChain: [],
  subjectChainLoading: false,
  subjectChainHasMore: false,
  subjectChainCursor: null,
  subjectDepthCapped: false,
  expandedEffects: {},
  loadingEffects: [],
};
