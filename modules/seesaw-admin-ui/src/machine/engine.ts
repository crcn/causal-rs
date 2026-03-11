import { BaseEvent, Dispatch } from "./events";

export type Engine<TState, TEvent extends BaseEvent<any, any>> = {
  handleEvent?: (event: TEvent, currState: TState, prevState: TState) => void;
  dispose: () => void;
};

export type EngineCreator<TState, TEvent extends BaseEvent<any, any>> = (
  dispatch: Dispatch<TEvent>,
  getState: () => TState
) => Engine<TState, TEvent>;

/**
 * Compose multiple engine creators into one.
 * Each engine receives every event — fan-out pattern.
 */
export const combineEngineCreators = <
  TState,
  TEvent extends BaseEvent<any, any>
>(
  ...creators: EngineCreator<TState, TEvent>[]
): EngineCreator<TState, TEvent> => {
  return (dispatch, getState) => {
    const engines = creators.map((create) => create(dispatch, getState));

    return {
      handleEvent: (event, currState, prevState) => {
        for (const engine of engines) {
          engine.handleEvent?.(event, currState, prevState);
        }
      },
      dispose: () => {
        for (const engine of engines) {
          engine.dispose();
        }
      },
    };
  };
};
