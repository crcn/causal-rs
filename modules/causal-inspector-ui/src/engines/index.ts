import { combineEngineCreators, type EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";
import { createSubscriptionEngine, type SubscriptionTransport } from "./subscription";
import { createQueryEngine, type QueryTransport } from "./query";
import { createStorageEngine, type StorageTransport } from "./storage";
import { createScrubberEngine } from "./scrubber";

export type { SubscriptionTransport } from "./subscription";
export type { QueryTransport } from "./query";
export type { StorageTransport } from "./storage";
export { createSubscriptionEngine } from "./subscription";
export { createQueryEngine } from "./query";
export { createStorageEngine } from "./storage";
export { createScrubberEngine } from "./scrubber";

export type InspectorTransport = SubscriptionTransport & QueryTransport;

/**
 * Create the default inspector engine combining subscription + query engines.
 *
 * Pass `storage` to persist pane layout changes. Load initial layout
 * via `CausalInspectorProvider`'s `initialState` prop instead.
 */
export const createInspectorEngine = (
  transport: InspectorTransport,
  storage?: StorageTransport
): EngineCreator<InspectorState, InspectorMachineEvent> => {
  const engines: EngineCreator<InspectorState, InspectorMachineEvent>[] = [
    createSubscriptionEngine(transport),
    createQueryEngine(transport),
    createScrubberEngine,
  ];
  if (storage) {
    engines.push(createStorageEngine(storage));
  }
  return combineEngineCreators(...engines);
};
