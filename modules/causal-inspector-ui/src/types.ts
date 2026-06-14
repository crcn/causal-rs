/** Processed event from the causal inspector backend. */
export type InspectorEvent = {
  seq: number;
  ts: string;
  type: string;
  name: string;
  id: string | null;
  parentId: string | null;
  workflowId: string | null;
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

export type ReactorAttempt = {
  eventId: string;
  reactorId: string;
  workflowId: string;
  attempt: number;
  status: string;
  error: string | null;
  startedAt: string;
  completedAt: string;
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
  workflowId: string;
  aggregateKey: string;
  state: unknown;
};

export type WorkflowSummary = {
  workflowId: string;
  eventCount: number;
  firstTs: string;
  lastTs: string;
  rootEventType: string;
  hasErrors: boolean;
};

export type WorkflowSummaryPage = {
  workflows: WorkflowSummary[];
  nextCursor: string | null;
};

export type FilterState = {
  search: string;
  workflowId: string | null;
  aggregateKey: string | null;
};

export type LogsFilter = {
  scope: "reactor" | "workflow";
  reactorId: string | null;
  workflowId: string | null;
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

// ── Entity-scoped inspection ──────────────────────────────────────────────

/** Whether a subject-chain event came from the entity's own stream or a downstream descendant. */
export type SubjectChainSourceMode = "stream" | "descendant";

/** Query mode for subject_chain — which events to include. */
export type SubjectChainMode = "stream" | "descendants" | "both";

/** One event in a subject chain response, with display names applied. */
export type SubjectChainEvent = {
  seq: number;
  ts: string;
  type: string;
  name: string;
  id: string | null;
  causationId: string | null;
  workflowId: string | null;
  reactorId: string | null;
  aggregateType: string | null;
  aggregateId: string | null;
  streamRevision: number | null;
  summary: string | null;
  payload: string;
  sourceMode: SubjectChainSourceMode;
};

/** One `ctx.effect()` result for the effects inspector panel. */
export type InspectorEffect = {
  consumer: string;
  label: string;
  value: unknown;
  createdAt: string;
};

/** One entity entry from the aggregate keys listing. */
export type AggregateKeyEntry = {
  aggregateId: string;
  displayLabel: string | null;
};

/** Paginated entity listing response. */
export type AggregateKeysPage = {
  entries: AggregateKeyEntry[];
  nextCursor: string | null;
};
