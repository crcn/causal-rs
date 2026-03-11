import type { BaseEvent } from "./machine";
import type {
  AdminEvent,
  FilterState,
  FlowSelection,
  HandlerDescription,
  HandlerLog,
  HandlerOutcome,
  LogsFilter,
} from "./types";

// ── Subscription events ──

type EventsReceived = BaseEvent<"events/received", AdminEvent[]>;
type SubscriptionConnected = BaseEvent<"events/subscription_connected">;
type SubscriptionError = BaseEvent<"events/subscription_error", { message: string }>;

// ── Query events ──

type PageLoaded = BaseEvent<
  "events/page_loaded",
  { events: AdminEvent[]; hasMore: boolean }
>;
type CausalTreeLoaded = BaseEvent<
  "events/causal_tree_loaded",
  { events: AdminEvent[]; rootSeq: number }
>;
type FlowLoaded = BaseEvent<"events/flow_loaded", AdminEvent[]>;
type LogsLoaded = BaseEvent<"events/logs_loaded", HandlerLog[]>;
type DescriptionsLoaded = BaseEvent<
  "events/descriptions_loaded",
  { runId: string; descriptions: HandlerDescription[] }
>;
type OutcomesLoaded = BaseEvent<
  "events/outcomes_loaded",
  { runId: string; outcomes: HandlerOutcome[] }
>;

// ── UI events ──

type EventSelected = BaseEvent<"ui/event_selected", { seq: number }>;
type EventDeselected = BaseEvent<"ui/event_deselected">;
type FlowOpened = BaseEvent<"ui/flow_opened", { runId: string }>;
type FlowClosed = BaseEvent<"ui/flow_closed">;
type FlowNodeSelected = BaseEvent<"ui/flow_node_selected", FlowSelection>;
type FilterChanged = BaseEvent<"ui/filter_changed", Partial<FilterState>>;
type LogsFilterChanged = BaseEvent<"ui/logs_filter_changed", Partial<LogsFilter>>;
type ScrubberMoved = BaseEvent<"ui/scrubber_moved", { position: number | null }>;
type LoadMoreRequested = BaseEvent<"ui/load_more_requested">;

// ── Flow graph events (computed) ──

type FlowGraphBuilt = BaseEvent<
  "flow/graph_built",
  { nodes: unknown[]; edges: unknown[] }
>;

// ── Union ──

export type AdminMachineEvent =
  | EventsReceived
  | SubscriptionConnected
  | SubscriptionError
  | PageLoaded
  | CausalTreeLoaded
  | FlowLoaded
  | LogsLoaded
  | DescriptionsLoaded
  | OutcomesLoaded
  | EventSelected
  | EventDeselected
  | FlowOpened
  | FlowClosed
  | FlowNodeSelected
  | FilterChanged
  | LogsFilterChanged
  | ScrubberMoved
  | LoadMoreRequested
  | FlowGraphBuilt;
