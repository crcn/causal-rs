import type {
  AdminEvent,
  FilterState,
  FlowSelection,
  HandlerDescription,
  HandlerLog,
  HandlerOutcome,
  LogsFilter,
} from "./types";

export type AdminState = {
  // Event stream
  events: AdminEvent[];
  hasMore: boolean;
  loading: boolean;

  // Selection
  selectedSeq: number | null;

  // Flow (causal DAG)
  flowRunId: string | null;
  flowData: AdminEvent[];
  flowSelection: FlowSelection;

  // Causal tree
  causalTree: { events: AdminEvent[]; rootSeq: number } | null;

  // Filters
  filters: FilterState;

  // Logs
  logs: HandlerLog[];
  logsFilter: LogsFilter;

  // Handler metadata (keyed by runId)
  descriptions: Record<string, HandlerDescription[]>;
  outcomes: Record<string, HandlerOutcome[]>;

  // Subscription status
  subscription: "connected" | "disconnected" | "error";
};

export const initialState: AdminState = {
  events: [],
  hasMore: true,
  loading: false,

  selectedSeq: null,

  flowRunId: null,
  flowData: [],
  flowSelection: null,

  causalTree: null,

  filters: {
    search: "",
    layers: [],
    from: null,
    to: null,
    runId: null,
  },

  logs: [],
  logsFilter: {
    scope: "handler",
    eventId: null,
    handlerId: null,
    runId: null,
  },

  descriptions: {},
  outcomes: {},

  subscription: "disconnected",
};
