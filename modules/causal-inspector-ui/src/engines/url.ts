import type { EngineCreator } from "../machine";
import type { InspectorMachineEvent } from "../events";
import type { InspectorState } from "../state";
import type { SubjectChainMode } from "../types";

type ParsedUrl = {
  workflowId: string | null;
  handler: string | null;
  subject: string | null;
  subjectMode: SubjectChainMode | null;
};

function parseUrl(): ParsedUrl {
  const params = new URLSearchParams(window.location.search);
  const subject = params.get("subject");
  const subjectModeRaw = params.get("subjectMode");
  const subjectMode: SubjectChainMode | null =
    subjectModeRaw === "stream" || subjectModeRaw === "descendants" || subjectModeRaw === "both"
      ? subjectModeRaw
      : null;

  if (subject) {
    return { workflowId: null, handler: null, subject, subjectMode };
  }
  return {
    workflowId: params.get("workflow"),
    handler: params.get("handler"),
    subject: null,
    subjectMode: null,
  };
}

function buildSearch(workflowId: string | null, handler: string | null): string {
  const params = new URLSearchParams();

  if (workflowId) {
    params.set("workflow", workflowId);
    if (handler) params.set("handler", handler);
  }

  const search = params.toString();
  return search ? `${window.location.pathname}?${search}` : window.location.pathname;
}

function buildSubjectSearch(aggregateType: string, aggregateId: string, mode: SubjectChainMode): string {
  const params = new URLSearchParams();
  params.set("subject", `${aggregateType}:${aggregateId}`);
  if (mode !== "both") params.set("subjectMode", mode);
  return `${window.location.pathname}?${params.toString()}`;
}

/**
 * URL engine — keeps the browser URL in sync with navigation state.
 *
 * Mutual exclusion: `?subject=` and `?workflow=` never coexist in the URL.
 * Navigating to a subject clears the workflow param and vice versa.
 *
 * For user-initiated actions, the reducer updates state directly.
 * This engine writes the URL as a side effect.
 *
 * For browser-initiated navigation (back/forward), this engine dispatches
 * location/changed so the reducer can update state from the URL.
 */
export const createUrlEngine: EngineCreator<InspectorState, InspectorMachineEvent> = (
  dispatch,
  _getState,
) => {
  const onPopState = () => {
    dispatch({ type: "location/changed", payload: parseUrl() });
  };
  window.addEventListener("popstate", onPopState);

  queueMicrotask(() => {
    const initial = parseUrl();
    if (initial.workflowId || initial.handler || initial.subject) {
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
          window.history.replaceState(
            null,
            "",
            buildSearch(
              new URLSearchParams(window.location.search).get("workflow"),
              event.payload.reactorId,
            ),
          );
          break;
        case "ui/subject_selected":
          window.history.pushState(
            null,
            "",
            buildSubjectSearch(
              event.payload.aggregateType,
              event.payload.aggregateId,
              event.payload.mode ?? "both",
            ),
          );
          break;
        case "ui/subject_mode_changed":
          window.history.replaceState(
            null,
            "",
            (() => {
              const params = new URLSearchParams(window.location.search);
              const existing = params.get("subject");
              if (!existing) return window.location.href;
              if (event.payload.mode === "both") params.delete("subjectMode");
              else params.set("subjectMode", event.payload.mode);
              return `${window.location.pathname}?${params.toString()}`;
            })(),
          );
          break;
      }
    },
    dispose: () => {
      window.removeEventListener("popstate", onPopState);
    },
  };
};
