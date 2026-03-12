import type { BaseEvent } from "./machine";
import type {
  InspectorEvent,
  FilterState,
  FlowSelection,
  ReactorDescription,
  ReactorLog,
  ReactorOutcome,
  LogsFilter,
  PaneLayout,
} from "./types";

// ── Subscription events ──

type EventsReceived = BaseEvent<"events/received", InspectorEvent[]>;
type SubscriptionConnected = BaseEvent<"events/subscription_connected">;
type SubscriptionError = BaseEvent<"events/subscription_error", { message: string }>;

// ── Query events ──

type PageLoaded = BaseEvent<
  "events/page_loaded",
  { events: InspectorEvent[]; hasMore: boolean }
>;
type CausalTreeLoaded = BaseEvent<
  "events/causal_tree_loaded",
  { events: InspectorEvent[]; rootSeq: number }
>;
type FlowLoaded = BaseEvent<"events/flow_loaded", InspectorEvent[]>;
type LogsLoaded = BaseEvent<"events/logs_loaded", ReactorLog[]>;
type DescriptionsLoaded = BaseEvent<
  "events/descriptions_loaded",
  { correlationId: string; descriptions: ReactorDescription[] }
>;
type OutcomesLoaded = BaseEvent<
  "events/outcomes_loaded",
  { correlationId: string; outcomes: ReactorOutcome[] }
>;

// ── UI events ──

type EventSelected = BaseEvent<"ui/event_selected", { seq: number }>;
type EventDeselected = BaseEvent<"ui/event_deselected">;
type FlowOpened = BaseEvent<"ui/flow_opened", { correlationId: string }>;
type FlowClosed = BaseEvent<"ui/flow_closed">;
type FlowNodeSelected = BaseEvent<"ui/flow_node_selected", FlowSelection>;
type FilterChanged = BaseEvent<"ui/filter_changed", Partial<FilterState>>;
type LogsFilterChanged = BaseEvent<"ui/logs_filter_changed", Partial<LogsFilter>>;
type LoadMoreRequested = BaseEvent<"ui/load_more_requested">;
type LayoutChanged = BaseEvent<"ui/layout_changed", PaneLayout>;

// ── Union ──

export type InspectorMachineEvent =
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
  | LoadMoreRequested
  | LayoutChanged;
