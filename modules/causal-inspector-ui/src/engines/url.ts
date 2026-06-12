import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";

function parseUrl(): { workflowId: string | null; handler: string | null } {
  const params = new URLSearchParams(window.location.search);
  return {
    workflowId: params.get("workflow"),
    handler: params.get("handler"),
  };
}

function buildSearch(workflowId: string | null, handler: string | null): string {
  const params = new URLSearchParams(window.location.search);

  if (workflowId) params.set("workflow", workflowId);
  else {
    params.delete("workflow");
    params.delete("handler");
  }

  if (handler && workflowId) params.set("handler", handler);
  else params.delete("handler");

  const search = params.toString();
  return search ? `${window.location.pathname}?${search}` : window.location.pathname;
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
    if (initial.workflowId || initial.handler) {
      dispatch({ type: "location/changed", payload: initial });
    }
  });

  return {
    handleEvent: (event) => {
      switch (event.type) {
        case "ui/flow_opened":
          window.history.pushState(null, "", buildSearch(event.payload.workflowId, null));
          break;
        case "ui/flow_closed":
          window.history.pushState(null, "", buildSearch(null, null));
          break;
        case "ui/filter_changed": {
          const payload = event.payload as Partial<{ workflowId: string | null }>;
          if (payload.workflowId !== undefined) {
            window.history.pushState(null, "", buildSearch(payload.workflowId, null));
          }
          break;
        }
        case "ui/handler_selected":
          // Replace rather than push — handler changes within a flow are fine as one history entry
          window.history.replaceState(
            null,
            "",
            buildSearch(
              new URLSearchParams(window.location.search).get("workflow"),
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
