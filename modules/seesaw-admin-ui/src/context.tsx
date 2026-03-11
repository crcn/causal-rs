import { useEffect, useMemo, type ReactNode } from "react";
import { Machine, MachineContext } from "./machine";
import { reducer } from "./reducer";
import { initialState, type AdminState } from "./state";
import type { AdminMachineEvent } from "./events";
import type { EngineCreator } from "./machine";

export type SeesawAdminConfig = {
  /** GraphQL endpoint URL */
  endpoint: string;
  /** WebSocket endpoint URL for subscriptions */
  wsEndpoint: string;
};

type SeesawAdminProviderProps = {
  config: SeesawAdminConfig;
  /** Optional custom engine creator (for domain-specific extensions) */
  createEngine?: EngineCreator<AdminState, AdminMachineEvent>;
  children: ReactNode;
};

const noopEngine: EngineCreator<AdminState, AdminMachineEvent> = () => ({
  dispose: () => {},
});

/**
 * Provides the seesaw admin machine to all child components.
 *
 * Usage:
 * ```tsx
 * <SeesawAdminProvider config={{ endpoint: "/graphql", wsEndpoint: "ws://..." }}>
 *   <TimelinePane />
 *   <CausalFlowPane />
 * </SeesawAdminProvider>
 * ```
 */
export function SeesawAdminProvider({
  config,
  createEngine = noopEngine,
  children,
}: SeesawAdminProviderProps) {
  const machine = useMemo(
    () => new Machine(reducer, createEngine, initialState),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    []
  );

  useEffect(() => {
    return () => machine.dispose();
  }, [machine]);

  return (
    <MachineContext.Provider value={machine}>
      {children}
    </MachineContext.Provider>
  );
}

/**
 * Hook to access the admin machine's state.
 * Re-exports useSelector and useDispatch typed for admin events.
 */
export { useSelector, useDispatch } from "./machine";
