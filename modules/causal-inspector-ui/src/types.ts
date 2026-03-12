/** Processed event from the causal inspector backend. */
export type InspectorEvent = {
  seq: number;
  ts: string;
  type: string;
  name: string;
  id: string | null;
  parentId: string | null;
  correlationId: string | null;
  reactorId: string | null;
  aggregateType: string | null;
  aggregateId: string | null;
  streamVersion: number | null;
  summary: string | null;
  payload: string;
};

export type InspectorEventsPage = {
  events: InspectorEvent[];
  nextCursor: number | null;
};

export type InspectorCausalTree = {
  events: InspectorEvent[];
  rootSeq: number;
};

export type InspectorCausalFlow = {
  events: InspectorEvent[];
};

export type ReactorLog = {
  eventId: string;
  reactorId: string;
  level: string;
  message: string;
  data: unknown;
  loggedAt: string;
};

/** Structured block within a reactor description (mirrors causal reactor DSL). */
export type Block =
  | { type: "label"; text: string }
  | { type: "counter"; label: string; value: number; total: number }
  | { type: "progress"; label: string; fraction: number }
  | { type: "checklist"; label: string; items: { text: string; done: boolean }[] }
  | { type: "key_value"; key: string; value: string }
  | { type: "status"; label: string; state: "waiting" | "running" | "done" | "error" };

export type ReactorDescription = {
  reactorId: string;
  blocks: Block[];
};

export type ReactorOutcome = {
  reactorId: string;
  status: string;
  error: string | null;
  attempts: number;
  startedAt: string | null;
  completedAt: string | null;
  triggeringEventIds: string[];
};

export type ReactorDescriptionSnapshot = {
  seq: number;
  eventId: string;
  reactorId: string;
  blocks: Block[];
};

export type AggregateStateEntry = {
  key: string;
  state: unknown;
};

export type AggregateTimelineEntry = {
  seq: number;
  eventId: string;
  eventType: string;
  aggregates: AggregateStateEntry[];
};

export type ReactorDependency = {
  reactorId: string;
  inputEventTypes: string[];
  outputEventTypes: string[];
};

export type AggregateLifecycleEntry = {
  seq: number;
  eventId: string;
  eventType: string;
  ts: string;
  correlationId: string;
  aggregateKey: string;
  state: unknown;
};

export type CorrelationSummary = {
  correlationId: string;
  eventCount: number;
  firstTs: string;
  lastTs: string;
  rootEventType: string;
  hasErrors: boolean;
};

export type CorrelationSummaryPage = {
  correlations: CorrelationSummary[];
  nextCursor: string | null;
};

export type FilterState = {
  search: string;
  correlationId: string | null;
  aggregateKey: string | null;
};

export type LogsFilter = {
  scope: "reactor" | "correlation";
  reactorId: string | null;
  correlationId: string | null;
};

export type FlowSelection =
  | { kind: "event-type"; name: string }
  | { kind: "reactor"; reactorId: string }
  | null;

/**
 * Serialized pane layout — opaque JSON structure stored in state.
 * Consumers (e.g. flexlayout-react) interpret this; the inspector
 * just stores and round-trips it.
 */
export type PaneLayout = Record<string, unknown>;
