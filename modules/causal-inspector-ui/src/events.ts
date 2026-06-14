import type { BaseEvent } from "./machine";
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
  PaneLayout,
  SubjectChainEvent,
  SubjectChainMode,
  InspectorEffect,
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
  { workflowId: string; descriptions: ReactorDescription[] }
>;
type DescriptionSnapshotsLoaded = BaseEvent<
  "events/description_snapshots_loaded",
  { workflowId: string; snapshots: ReactorDescriptionSnapshot[] }
>;
type AggregateTimelineLoaded = BaseEvent<
  "events/aggregate_timeline_loaded",
  { workflowId: string; entries: AggregateTimelineEntry[] }
>;
type OutcomesLoaded = BaseEvent<
  "events/outcomes_loaded",
  { workflowId: string; outcomes: ReactorOutcome[] }
>;
type AttemptsLoaded = BaseEvent<
  "events/attempts_loaded",
  { workflowId: string; attempts: ReactorAttempt[] }
>;
type WorkflowsLoaded = BaseEvent<"events/workflows_loaded", { workflows: WorkflowSummary[]; hasMore: boolean; append: boolean }>;
type ReactorDependenciesLoaded = BaseEvent<"events/reactor_dependencies_loaded", ReactorDependency[]>;
type AggregateKeysLoaded = BaseEvent<"events/aggregate_keys_loaded", string[]>;
type AggregateLifecycleLoaded = BaseEvent<"events/aggregate_lifecycle_loaded", { key: string; entries: AggregateLifecycleEntry[] }>;

// ── UI events ──

type EventSelected = BaseEvent<"ui/event_selected", { seq: number }>;
type EventDeselected = BaseEvent<"ui/event_deselected">;
type FlowOpened = BaseEvent<"ui/flow_opened", { workflowId: string }>;
type FlowClosed = BaseEvent<"ui/flow_closed">;
type FlowNodeSelected = BaseEvent<"ui/flow_node_selected", FlowSelection>;
type FilterChanged = BaseEvent<"ui/filter_changed", Partial<FilterState>>;
type LoadMoreRequested = BaseEvent<"ui/load_more_requested">;
type LayoutChanged = BaseEvent<"ui/layout_changed", PaneLayout>;
type ScrubberStartChanged = BaseEvent<"ui/scrubber_start_changed", { start: number | null }>;
type ScrubberEndChanged = BaseEvent<"ui/scrubber_end_changed", { end: number | null }>;
type ScrubberPlayToggled = BaseEvent<"ui/scrubber_play_toggled">;
type ScrubberSpeedChanged = BaseEvent<"ui/scrubber_speed_changed", { speed: number }>;
type WorkflowsRequested = BaseEvent<"ui/workflows_requested", { search?: string }>;
type HandlerSelected = BaseEvent<"ui/handler_selected", { reactorId: string }>;
type AggregateLifecycleRequested = BaseEvent<"ui/aggregate_lifecycle_requested", { aggregateKey: string }>;
type LoadMoreWorkflowsRequested = BaseEvent<"ui/load_more_workflows_requested">;
type LocationChanged = BaseEvent<"location/changed", { workflowId: string | null; handler: string | null; subject: string | null; subjectMode: SubjectChainMode | null }>;

// ── Entity-scoped inspection events ──

type SubjectSelected = BaseEvent<"ui/subject_selected", { aggregateType: string; aggregateId: string; mode?: SubjectChainMode }>;
type SubjectModeChanged = BaseEvent<"ui/subject_mode_changed", { mode: SubjectChainMode }>;
type SubjectChainLoadMore = BaseEvent<"ui/subject_chain_load_more">;
type EventEffectsRequested = BaseEvent<"ui/event_effects_requested", { eventId: string }>;
type SubjectChainLoaded = BaseEvent<"events/subject_chain_loaded", { events: SubjectChainEvent[]; hasMore: boolean; cursor: number | null; depthCapped: boolean; append: boolean }>;
type EventEffectsLoaded = BaseEvent<"events/event_effects_loaded", { eventId: string; effects: InspectorEffect[] }>;

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
  | DescriptionSnapshotsLoaded
  | AggregateTimelineLoaded
  | OutcomesLoaded
  | AttemptsLoaded
  | WorkflowsLoaded
  | EventSelected
  | EventDeselected
  | FlowOpened
  | FlowClosed
  | FlowNodeSelected
  | FilterChanged
  | LoadMoreRequested
  | LayoutChanged
  | ScrubberStartChanged
  | ScrubberEndChanged
  | ScrubberPlayToggled
  | ScrubberSpeedChanged
  | WorkflowsRequested
  | ReactorDependenciesLoaded
  | AggregateKeysLoaded
  | HandlerSelected
  | LocationChanged
  | AggregateLifecycleLoaded
  | AggregateLifecycleRequested
  | LoadMoreWorkflowsRequested
  | SubjectSelected
  | SubjectModeChanged
  | SubjectChainLoadMore
  | EventEffectsRequested
  | SubjectChainLoaded
  | EventEffectsLoaded;
