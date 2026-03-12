import { produce, type Draft } from "immer";

type ChangeListener<TState> = (
  newState: TState,
  oldState: TState
) => void;

export type Reducer<TState, TEvent extends { type: string }> = (
  draft: Draft<TState>,
  event: TEvent
) => void;

/**
 * Immutable state store powered by Immer.
 *
 * Reducer receives a mutable draft (Immer proxy) — mutations are
 * automatically converted to structural sharing under the hood.
 */
export class Store<TState, TEvent extends { type: string }> {
  private _state: TState;
  private _listeners = new Set<ChangeListener<TState>>();

  constructor(
    private _reduce: Reducer<TState, TEvent>,
    initialState: TState
  ) {
    this._state = initialState;
  }

  getState(): TState {
    return this._state;
  }

  dispatch(event: TEvent) {
    const oldState = this._state;
    this._state = produce(oldState, (draft) => {
      this._reduce(draft as Draft<TState>, event);
    });
    if (this._state !== oldState) {
      for (const listener of this._listeners) {
        listener(this._state, oldState);
      }
    }
  }

  /**
   * Subscribe to state changes. Returns an unsubscribe function.
   */
  onStateChange(listener: ChangeListener<TState>): () => void {
    this._listeners.add(listener);
    return () => {
      this._listeners.delete(listener);
    };
  }

  dispose() {
    this._listeners.clear();
  }
}
