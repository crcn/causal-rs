import { useEffect, useMemo, type ReactNode } from "react";
import { Machine, MachineContext } from "./machine";
import { reducer } from "./reducer";
import { initialState, type AdminState } from "./state";
import type { AdminMachineEvent } from "./events";
import type { EngineCreator } from "./machine";

type SeesawAdminProviderProps = {
  createEngine: EngineCreator<AdminState, AdminMachineEvent>;
  children: ReactNode;
};

/**
 * Provides the seesaw admin machine to all child components.
 *
 * Usage:
 * ```tsx
 * <SeesawAdminProvider createEngine={createAdminEngine(transport)}>
 *   <TimelinePane />
 *   <CausalFlowPane />
 * </SeesawAdminProvider>
 * ```
 */
export function SeesawAdminProvider({
  createEngine,
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
