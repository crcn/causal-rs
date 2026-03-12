import { useEffect, useMemo, type ReactNode } from "react";
import { Machine, MachineContext } from "./machine";
import { reducer } from "./reducer";
import { initialState, type InspectorState } from "./state";
import type { InspectorMachineEvent } from "./events";
import type { EngineCreator } from "./machine";

type CausalInspectorProviderProps = {
  createEngine: EngineCreator<InspectorState, InspectorMachineEvent>;
  /** Override slices of initial state (e.g. hydrated paneLayout). */
  initialState?: Partial<InspectorState>;
  children: ReactNode;
};

/**
 * Provides the causal inspector machine to all child components.
 *
 * Usage:
 * ```tsx
 * <CausalInspectorProvider
 *   createEngine={createInspectorEngine(transport, storage)}
 *   initialState={{ paneLayout: savedLayout }}
 * >
 *   <TimelinePane />
 *   <CausalFlowPane />
 * </CausalInspectorProvider>
 * ```
 */
export function CausalInspectorProvider({
  createEngine,
  initialState: overrides,
  children,
}: CausalInspectorProviderProps) {
  const machine = useMemo(
    () =>
      new Machine(
        reducer,
        createEngine,
        overrides ? { ...initialState, ...overrides } : initialState
      ),
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
