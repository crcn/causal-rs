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

// ── Causal Admin domain ──
export { CausalAdminProvider } from "./context";
export { reducer } from "./reducer";
export { initialState } from "./state";
export type { AdminState } from "./state";
export type { AdminMachineEvent } from "./events";

// ── Types ──
export type {
  AdminEvent,
  AdminEventsPage,
  AdminCausalTree,
  AdminCausalFlow,
  Block,
  HandlerLog,
  HandlerDescription,
  HandlerOutcome,
  FilterState,
  LogsFilter,
  FlowSelection,
} from "./types";

// ── Engines ──
export {
  createAdminEngine,
  createSubscriptionEngine,
  createQueryEngine,
} from "./engines";
export type {
  AdminTransport,
  SubscriptionTransport,
  QueryTransport,
} from "./engines";

// ── Queries ──
export {
  EVENTS_SUBSCRIPTION,
  ADMIN_EVENTS,
  ADMIN_CAUSAL_TREE,
  ADMIN_CAUSAL_FLOW,
  ADMIN_HANDLER_LOGS,
  ADMIN_HANDLER_LOGS_BY_RUN,
  ADMIN_HANDLER_DESCRIPTIONS,
  ADMIN_HANDLER_OUTCOMES,
} from "./queries";

// ── Theme ──
export {
  eventHue,
  eventBg,
  eventBorder,
  eventTextColor,
  LAYER_COLORS,
  LOG_LEVEL_COLORS,
  LAYER_OPTIONS,
} from "./theme";

// ── Utilities ──
export { formatTs, compactPayload, copyToClipboard } from "./utils";

// ── Components ──
export { FilterBar } from "./components/FilterBar";
export { CopyablePayload } from "./components/CopyablePayload";
export { JsonSyntax } from "./components/JsonSyntax";

// ── Panes ──
export { TimelinePane } from "./panes/TimelinePane";
export type { TimelinePaneProps } from "./panes/TimelinePane";
export { CausalTreePane } from "./panes/CausalTreePane";
export type { CausalTreePaneProps } from "./panes/CausalTreePane";
export { LogsPane } from "./panes/LogsPane";
export type { LogsPaneProps } from "./panes/LogsPane";
export { CausalFlowPane } from "./panes/CausalFlowPane";
export type { CausalFlowPaneProps } from "./panes/CausalFlowPane";
