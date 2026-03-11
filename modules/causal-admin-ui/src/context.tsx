import { useEffect, useMemo, type ReactNode } from "react";
import { Machine, MachineContext } from "./machine";
import { reducer } from "./reducer";
import { initialState, type AdminState } from "./state";
import type { AdminMachineEvent } from "./events";
import type { EngineCreator } from "./machine";

type CausalAdminProviderProps = {
  createEngine: EngineCreator<AdminState, AdminMachineEvent>;
  children: ReactNode;
};

/**
 * Provides the causal admin machine to all child components.
 *
 * Usage:
 * ```tsx
 * <CausalAdminProvider createEngine={createAdminEngine(transport)}>
 *   <TimelinePane />
 *   <CausalFlowPane />
 * </CausalAdminProvider>
 * ```
 */
export function CausalAdminProvider({
  createEngine,
  children,
}: CausalAdminProviderProps) {
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
