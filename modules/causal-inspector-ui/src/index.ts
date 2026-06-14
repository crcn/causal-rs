// ── Machine infra (generic, reusable) ──
export {
  Machine,
  Store,
  combineEngineCreators,
  MachineContext,
  useSelector,
  useDispatch,
  useInlineMachine,
} from "./machine";
export type {
  Reducer,
  Engine,
  EngineCreator,
  BaseEvent,
  Dispatch,
} from "./machine";

// ── Causal Inspector domain ──
export { CausalInspectorProvider } from "./context";
export { reducer } from "./reducer";
export { initialState } from "./state";
export type { InspectorState } from "./state";
export type { InspectorMachineEvent } from "./events";

// ── Types ──
export type {
  InspectorEvent,
  InspectorEventsPage,
  InspectorCausalTree,
  InspectorCausalFlow,
  WorkflowSummary,
  ReactorDependency,
  AggregateLifecycleEntry,
  Block,
  ReactorLog,
  ReactorDescription,
  ReactorDescriptionSnapshot,
  AggregateStateEntry,
  AggregateTimelineEntry,
  ReactorOutcome,
  ReactorAttempt,
  FilterState,
  LogsFilter,
  FlowSelection,
  PaneLayout,
  SubjectChainSourceMode,
  SubjectChainMode,
  SubjectChainEvent,
  InspectorEffect,
  AggregateKeyEntry,
  AggregateKeysPage,
} from "./types";

// ── Engines ──
export {
  createInspectorEngine,
  createSubscriptionEngine,
  createQueryEngine,
  createStorageEngine,
} from "./engines";
export type {
  InspectorTransport,
  SubscriptionTransport,
  QueryTransport,
  StorageTransport,
} from "./engines";

// ── Queries ──
export {
  EVENTS_SUBSCRIPTION,
  INSPECTOR_EVENTS,
  INSPECTOR_CAUSAL_TREE,
  INSPECTOR_CAUSAL_FLOW,
  INSPECTOR_WORKFLOWS,
  INSPECTOR_REACTOR_LOGS,
  INSPECTOR_REACTOR_LOGS_BY_WORKFLOW,
  INSPECTOR_REACTOR_DESCRIPTIONS,
  INSPECTOR_REACTOR_DESCRIPTION_SNAPSHOTS,
  INSPECTOR_AGGREGATE_TIMELINE,
  INSPECTOR_REACTOR_OUTCOMES,
  INSPECTOR_REACTOR_ATTEMPTS,
  INSPECTOR_REACTOR_DEPENDENCIES,
  INSPECTOR_AGGREGATE_KEYS,
  INSPECTOR_AGGREGATE_LIFECYCLE,
  INSPECTOR_SUBJECT_CHAIN,
  INSPECTOR_EFFECTS_FOR_EVENT,
  INSPECTOR_AGGREGATE_TYPES,
  INSPECTOR_AGGREGATE_KEYS_BY_TYPE,
} from "./queries";

// ── Theme ──
export {
  eventHue,
  eventBg,
  eventBorder,
  eventTextColor,
  LOG_LEVEL_COLORS,
} from "./theme";

// ── Utilities ──
export { formatTs, compactPayload, copyToClipboard } from "./utils";

// ── Drop-in component ──
export { CausalInspector } from "./CausalInspector";
export type { CausalInspectorProps } from "./CausalInspector";

// ── Components ──
export { FilterBar } from "./components/FilterBar";
export { CopyablePayload } from "./components/CopyablePayload";
export { JsonSyntax } from "./components/JsonSyntax";
export { GlobalScrubber } from "./components/GlobalScrubber";
export { EffectList } from "./components/EffectList";
// ── Panes ──
export { TimelinePane } from "./panes/TimelinePane";
export type { TimelinePaneProps } from "./panes/TimelinePane";
export { CausalTreePane } from "./panes/CausalTreePane";
export type { CausalTreePaneProps } from "./panes/CausalTreePane";
export { LogsPane } from "./panes/LogsPane";
export type { LogsPaneProps } from "./panes/LogsPane";
export { CausalFlowPane } from "./panes/CausalFlowPane";
export type { CausalFlowPaneProps } from "./panes/CausalFlowPane";
export { AggregateTimelinePane } from "./panes/AggregateTimelinePane";
export { WaterfallPane } from "./panes/WaterfallPane";
export { WorkflowExplorerPane } from "./panes/WorkflowExplorerPane";
export { SubjectChainPane } from "./panes/SubjectChainPane";

