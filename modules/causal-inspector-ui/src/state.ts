import type {
  InspectorEvent,
  CorrelationSummary,
  ReactorDependency,
  AggregateLifecycleEntry,
  FilterState,
  FlowSelection,
  ReactorDescription,
  ReactorDescriptionSnapshot,
  AggregateTimelineEntry,
  ReactorLog,
  ReactorOutcome,
  LogsFilter,
  PaneLayout,
} from "./types";

export type InspectorState = {
  // Event stream
  events: InspectorEvent[];
  hasMore: boolean;
  loading: boolean;

  // Selection
  selectedSeq: number | null;

  // Flow (causal DAG) — keyed by correlation_id
  flowCorrelationId: string | null;
  flowData: InspectorEvent[];
  flowSelection: FlowSelection;

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

  // Reactor metadata (keyed by correlationId)
  descriptions: Record<string, ReactorDescription[]>;
  descriptionSnapshots: Record<string, ReactorDescriptionSnapshot[]>;
  aggregateTimeline: Record<string, AggregateTimelineEntry[]>;
  outcomes: Record<string, ReactorOutcome[]>;

  // Correlations
  correlations: CorrelationSummary[];
  correlationsLoading: boolean;
  correlationsHasMore: boolean;

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
};

export const initialState: InspectorState = {
  events: [],
  hasMore: true,
  loading: false,

  selectedSeq: null,

  flowCorrelationId: null,
  flowData: [],
  flowSelection: null,

  scrubberStart: null,
  scrubberEnd: null,
  scrubberPlaying: false,
  scrubberSpeed: 300,

  causalTree: null,

  filters: {
    search: "",
    correlationId: null,
    aggregateKey: null,
  },

  logs: [],
  logsFilter: {
    scope: "reactor",
    reactorId: null,
    correlationId: null,
  },

  descriptions: {},
  descriptionSnapshots: {},
  aggregateTimeline: {},
  outcomes: {},

  correlations: [],
  correlationsLoading: false,
  correlationsHasMore: true,

  reactorDependencies: [],

  aggregateKeys: [],
  aggregateLifecycle: [],
  aggregateLifecycleKey: null,

  subscription: "disconnected",

  paneLayout: null,
};
