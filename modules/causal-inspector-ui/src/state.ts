import type {
  InspectorEvent,
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

  // Time scrubber — null means show all events
  scrubberPosition: number | null;
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

  scrubberPosition: null,
  scrubberPlaying: false,
  scrubberSpeed: 300,

  causalTree: null,

  filters: {
    search: "",
    from: null,
    to: null,
    correlationId: null,
  },

  logs: [],
  logsFilter: {
    scope: "reactor",
    eventId: null,
    reactorId: null,
    correlationId: null,
  },

  descriptions: {},
  descriptionSnapshots: {},
  aggregateTimeline: {},
  outcomes: {},

  subscription: "disconnected",

  paneLayout: null,
};
