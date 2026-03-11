/** Processed event from the causal admin backend. */
export type AdminEvent = {
  seq: number;
  ts: string;
  type: string;
  name: string;
  layer: string;
  id: string | null;
  parentId: string | null;
  correlationId: string | null;
  runId: string | null;
  reactorId: string | null;
  summary: string | null;
  payload: string;
};

export type AdminEventsPage = {
  events: AdminEvent[];
  nextCursor: number | null;
};

export type AdminCausalTree = {
  events: AdminEvent[];
  rootSeq: number;
};

export type AdminCausalFlow = {
  events: AdminEvent[];
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

export type FilterState = {
  search: string;
  layers: string[];
  from: string | null;
  to: string | null;
  runId: string | null;
};

export type LogsFilter = {
  scope: "reactor" | "run";
  eventId: string | null;
  reactorId: string | null;
  runId: string | null;
};

export type FlowSelection =
  | { kind: "event-type"; reactorId: string | null; name: string }
  | { kind: "reactor"; reactorId: string }
  | null;
