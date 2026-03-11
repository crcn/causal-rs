/** Processed event from the seesaw admin backend. */
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
  handlerId: string | null;
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

export type HandlerLog = {
  eventId: string;
  handlerId: string;
  level: string;
  message: string;
  data: unknown | null;
  loggedAt: string;
};

export type HandlerDescription = {
  handlerId: string;
  blocks: unknown;
};

export type HandlerOutcome = {
  handlerId: string;
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
  scope: "handler" | "run";
  eventId: string | null;
  handlerId: string | null;
  runId: string | null;
  levels: string[];
  search: string;
};

export type FlowSelection = {
  nodeId: string | null;
  eventSeq: number | null;
};
