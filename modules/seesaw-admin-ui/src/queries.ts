/** GraphQL query and subscription documents for the seesaw admin API. */

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
  handlerId
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

export const ADMIN_HANDLER_LOGS = `
  query AdminHandlerLogs($eventId: String!, $handlerId: String!) {
    adminHandlerLogs(eventId: $eventId, handlerId: $handlerId) {
      eventId
      handlerId
      level
      message
      data
      loggedAt
    }
  }
`;

export const ADMIN_HANDLER_LOGS_BY_RUN = `
  query AdminHandlerLogsByRun($runId: String!) {
    adminHandlerLogsByRun(runId: $runId) {
      eventId
      handlerId
      level
      message
      data
      loggedAt
    }
  }
`;

export const ADMIN_HANDLER_DESCRIPTIONS = `
  query AdminHandlerDescriptions($runId: String!) {
    adminHandlerDescriptions(runId: $runId) {
      handlerId
      blocks
    }
  }
`;

export const ADMIN_HANDLER_OUTCOMES = `
  query AdminHandlerOutcomes($runId: String!) {
    adminHandlerOutcomes(runId: $runId) {
      handlerId
      status
      error
      attempts
      startedAt
      completedAt
      triggeringEventIds
    }
  }
`;
