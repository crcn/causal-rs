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

// ── Seesaw Admin domain ──
export { SeesawAdminProvider } from "./context";
export type { SeesawAdminConfig } from "./context";
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
  HandlerLog,
  HandlerDescription,
  HandlerOutcome,
  FilterState,
  LogsFilter,
  FlowSelection,
} from "./types";
