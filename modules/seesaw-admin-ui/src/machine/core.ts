import { Engine, EngineCreator } from "./engine";
import { Reducer, Store } from "./store";

/**
 * Machine = Store (pure state) + Engine (side effects).
 *
 * Dispatch flow:
 *   UI dispatches event
 *   → Store reduces (sync, via Immer)
 *   → Engine handles event with (curr, prev) state
 *   → Engine may dispatch new events (async reactions)
 */
export class Machine<TState, TEvent extends { type: string }> {
  private _engine: Engine<TState, TEvent>;
  private _store: Store<TState, TEvent>;

  constructor(
    reducer: Reducer<TState, TEvent>,
    createEngine: EngineCreator<TState, TEvent>,
    initialState: TState
  ) {
    this._store = new Store(reducer, initialState);
    this._engine = createEngine(this.dispatch, this.getState);
  }

  getState = (): TState => {
    return this._store.getState();
  };

  dispatch = (event: TEvent): void => {
    const prevState = this._store.getState();
    this._store.dispatch(event);
    const currState = this._store.getState();
    this._engine.handleEvent?.(event, currState, prevState);
  };

  onStateChange(listener: (newState: TState, oldState: TState) => void) {
    return this._store.onStateChange(listener);
  }

  dispose() {
    this._engine.dispose();
    this._store.dispose();
  }
}
