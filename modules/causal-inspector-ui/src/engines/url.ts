import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";

function parseUrl(): { correlationId: string | null; handler: string | null } {
  const params = new URLSearchParams(window.location.search);
  return {
    correlationId: params.get("correlation"),
    handler: params.get("handler"),
  };
}

function buildSearch(correlationId: string | null, handler: string | null): string {
  const params = new URLSearchParams(window.location.search);

  if (correlationId) params.set("correlation", correlationId);
  else {
    params.delete("correlation");
    params.delete("handler");
  }

  if (handler && correlationId) params.set("handler", handler);
  else params.delete("handler");

  const search = params.toString();
  return search ? `?${search}` : window.location.pathname;
}

/**
 * URL engine — keeps the browser URL in sync with navigation state.
 *
 * For user-initiated actions (ui/flow_opened, etc.), the reducer updates
 * state directly. This engine just writes the URL as a side effect.
 *
 * For browser-initiated navigation (back/forward), this engine dispatches
 * location/changed so the reducer can update state from the URL.
 */
export const createUrlEngine: EngineCreator<InspectorState, InspectorMachineEvent> = (
  dispatch,
  _getState,
) => {
  // Popstate — browser back/forward
  const onPopState = () => {
    dispatch({ type: "location/changed", payload: parseUrl() });
  };
  window.addEventListener("popstate", onPopState);

  // Seed from current URL on init — deferred so the Machine constructor finishes first.
  queueMicrotask(() => {
    const initial = parseUrl();
    if (initial.correlationId || initial.handler) {
      dispatch({ type: "location/changed", payload: initial });
    }
  });

  return {
    handleEvent: (event) => {
      switch (event.type) {
        case "ui/flow_opened":
          window.history.pushState(null, "", buildSearch(event.payload.correlationId, null));
          break;
        case "ui/flow_closed":
          window.history.pushState(null, "", buildSearch(null, null));
          break;
        case "ui/handler_selected":
          // Replace rather than push — handler changes within a flow are fine as one history entry
          window.history.replaceState(
            null,
            "",
            buildSearch(
              new URLSearchParams(window.location.search).get("correlation"),
              event.payload.reactorId,
            ),
          );
          break;
      }
    },
    dispose: () => {
      window.removeEventListener("popstate", onPopState);
    },
  };
};
