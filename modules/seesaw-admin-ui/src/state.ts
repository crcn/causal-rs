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
  flowGraph: { nodes: unknown[]; edges: unknown[] } | null;
  flowSelection: FlowSelection;
  scrubberPosition: number | null;

  // Causal tree
  causalTree: { events: AdminEvent[]; rootSeq: number } | null;
  treeSource: "dedicated" | "flow";

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
  flowGraph: null,
  flowSelection: { nodeId: null, eventSeq: null },
  scrubberPosition: null,

  causalTree: null,
  treeSource: "dedicated",

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
    levels: ["info", "warn", "error"],
    search: "",
  },

  descriptions: {},
  outcomes: {},

  subscription: "disconnected",
};
