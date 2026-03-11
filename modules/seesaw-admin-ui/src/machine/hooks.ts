import { createContext, useContext, useEffect, useMemo, useRef, useState } from "react";
import { Machine } from "./core";
import { BaseEvent, Dispatch } from "./events";
import { EngineCreator } from "./engine";
import { Reducer } from "./store";

export const MachineContext = createContext<Machine<any, any> | null>(null);

const useMachine = <TState, TEvent extends BaseEvent<any, any>>(): Machine<
  TState,
  TEvent
> => {
  const machine = useContext(MachineContext);
  if (!machine) {
    throw new Error("useMachine must be used within a MachineContext.Provider");
  }
  return machine;
};

/**
 * Select a slice of state. Only re-renders when the selected value changes
 * (reference equality).
 */
export const useSelector = <TState, TSelected>(
  selector: (state: TState) => TSelected
): TSelected => {
  const machine = useMachine<TState, any>();
  const [value, setValue] = useState(() => selector(machine.getState()));
  const selectorRef = useRef(selector);
  selectorRef.current = selector;

  useEffect(() => {
    // Sync in case state changed between render and effect
    setValue(selectorRef.current(machine.getState()));

    return machine.onStateChange((newState) => {
      setValue(selectorRef.current(newState as TState));
    });
  }, [machine]);

  return value;
};

/**
 * Get the dispatch function. Stable reference — never causes re-renders.
 */
export const useDispatch = <
  TEvent extends BaseEvent<any, any>
>(): Dispatch<TEvent> => {
  return useMachine<any, TEvent>().dispatch;
};

/**
 * Create and manage a machine inline within a component.
 * Useful for self-contained widgets.
 */
export const useInlineMachine = <TState, TEvent extends BaseEvent<any, any>>(
  reducer: Reducer<TState, TEvent>,
  createEngine: EngineCreator<TState, TEvent>,
  initialState: TState
): [TState, Dispatch<TEvent>] => {
  const machine = useMemo(
    () => new Machine(reducer, createEngine, initialState),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    []
  );

  const [state, setState] = useState(machine.getState());

  useEffect(() => {
    const unsub = machine.onStateChange((newState) => setState(newState));
    return () => {
      unsub();
      machine.dispose();
    };
  }, [machine]);

  return [state, machine.dispatch];
};
