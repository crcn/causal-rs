import { combineEngineCreators, type EngineCreator } from "../machine";
import type { AdminMachineEvent } from "../events";
import type { AdminState } from "../state";
import { createSubscriptionEngine, type SubscriptionTransport } from "./subscription";
import { createQueryEngine, type QueryTransport } from "./query";

export type { SubscriptionTransport } from "./subscription";
export type { QueryTransport } from "./query";
export { createSubscriptionEngine } from "./subscription";
export { createQueryEngine } from "./query";

export type AdminTransport = SubscriptionTransport & QueryTransport;

/**
 * Create the default admin engine combining subscription + query engines.
 *
 * Usage:
 * ```ts
 * const engine = createAdminEngine(myTransport);
 * // or compose with your own engines:
 * const engine = combineEngineCreators(createAdminEngine(transport), myCustomEngine);
 * ```
 */
export const createAdminEngine = (
  transport: AdminTransport
): EngineCreator<AdminState, AdminMachineEvent> => {
  return combineEngineCreators(
    createSubscriptionEngine(transport),
    createQueryEngine(transport)
  );
};
