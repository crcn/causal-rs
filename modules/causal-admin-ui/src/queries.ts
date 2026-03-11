/** GraphQL query and subscription documents for the causal admin API. */

const ADMIN_EVENT_FIELDS = `
  seq
  ts
  type
  name
  layer
  id
  parentId
  correlationId
  runId
  reactorId
  summary
  payload
`;

export const EVENTS_SUBSCRIPTION = `
  subscription Events($lastSeq: Int) {
    adminEventAdded(lastSeq: $lastSeq) {
      ${ADMIN_EVENT_FIELDS}
    }
  }
`;

export const ADMIN_EVENTS = `
  query AdminEvents(
    $limit: Int!
    $cursor: Int
    $search: String
    $from: DateTime
    $to: DateTime
    $runId: String
  ) {
    adminEvents(
      limit: $limit
      cursor: $cursor
      search: $search
      from: $from
      to: $to
      runId: $runId
    ) {
      events {
        ${ADMIN_EVENT_FIELDS}
      }
      nextCursor
    }
  }
`;

export const ADMIN_CAUSAL_TREE = `
  query AdminCausalTree($seq: Int!) {
    adminCausalTree(seq: $seq) {
      events {
        ${ADMIN_EVENT_FIELDS}
      }
      rootSeq
    }
  }
`;

export const ADMIN_CAUSAL_FLOW = `
  query AdminCausalFlow($runId: String!) {
    adminCausalFlow(runId: $runId) {
      events {
        ${ADMIN_EVENT_FIELDS}
      }
    }
  }
`;

export const ADMIN_REACTOR_LOGS = `
  query AdminReactorLogs($eventId: String!, $reactorId: String!) {
    adminReactorLogs(eventId: $eventId, reactorId: $reactorId) {
      eventId
      reactorId
      level
      message
      data
      loggedAt
    }
  }
`;

export const ADMIN_REACTOR_LOGS_BY_RUN = `
  query AdminReactorLogsByRun($runId: String!) {
    adminReactorLogsByRun(runId: $runId) {
      eventId
      reactorId
      level
      message
      data
      loggedAt
    }
  }
`;

export const ADMIN_REACTOR_DESCRIPTIONS = `
  query AdminReactorDescriptions($runId: String!) {
    adminReactorDescriptions(runId: $runId) {
      reactorId
      blocks
    }
  }
`;

export const ADMIN_REACTOR_OUTCOMES = `
  query AdminReactorOutcomes($runId: String!) {
    adminReactorOutcomes(runId: $runId) {
      reactorId
      status
      error
      attempts
      startedAt
      completedAt
      triggeringEventIds
    }
  }
`;
