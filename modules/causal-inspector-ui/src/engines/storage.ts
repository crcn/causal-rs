import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";
import type { PaneLayout } from "../types";

export type StorageTransport = {
  /** Persist layout to storage. */
  saveLayout: (layout: PaneLayout) => void;
};

/**
 * Storage engine — persists pane layout changes as a side effect.
 *
 * Initial layout should be loaded by the consumer and passed via
 * the provider's `initialState` — not dispatched from here.
 */
export const createStorageEngine = (
  transport: StorageTransport
): EngineCreator<InspectorState, InspectorMachineEvent> => {
  return () => ({
    handleEvent: (event) => {
      if (event.type === "ui/layout_changed") {
        transport.saveLayout(event.payload);
      }
    },
    dispose: () => {},
  });
};
