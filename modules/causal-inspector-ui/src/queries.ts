/** GraphQL query and subscription documents for the causal inspector API. */

const EVENT_FIELDS = `
  seq
  ts
  type
  name
  id
  parentId
  correlationId
  reactorId
  aggregateType
  aggregateId
  streamVersion
  summary
  payload
`;

export const EVENTS_SUBSCRIPTION = `
  subscription Events($lastSeq: Int) {
    inspectorEventAdded(lastSeq: $lastSeq) {
      ${EVENT_FIELDS}
    }
  }
`;

export const INSPECTOR_EVENTS = `
  query InspectorEvents(
    $limit: Int!
    $cursor: Int
    $search: String
    $correlationId: String
    $aggregateKey: String
  ) {
    inspectorEvents(
      limit: $limit
      cursor: $cursor
      search: $search
      correlationId: $correlationId
      aggregateKey: $aggregateKey
    ) {
      events {
        ${EVENT_FIELDS}
      }
      nextCursor
    }
  }
`;

export const INSPECTOR_CAUSAL_TREE = `
  query InspectorCausalTree($seq: Int!) {
    inspectorCausalTree(seq: $seq) {
      events {
        ${EVENT_FIELDS}
      }
      rootSeq
    }
  }
`;

export const INSPECTOR_CAUSAL_FLOW = `
  query InspectorCausalFlow($correlationId: String!) {
    inspectorCausalFlow(correlationId: $correlationId) {
      events {
        ${EVENT_FIELDS}
      }
    }
  }
`;

export const INSPECTOR_REACTOR_LOGS = `
  query InspectorReactorLogs($eventId: String!, $reactorId: String!) {
    inspectorReactorLogs(eventId: $eventId, reactorId: $reactorId) {
      eventId
      reactorId
      level
      message
      data
      loggedAt
    }
  }
`;

export const INSPECTOR_REACTOR_LOGS_BY_CORRELATION = `
  query InspectorReactorLogsByCorrelation($correlationId: String!) {
    inspectorReactorLogsByCorrelation(correlationId: $correlationId) {
      eventId
      reactorId
      level
      message
      data
      loggedAt
    }
  }
`;

export const INSPECTOR_REACTOR_DESCRIPTIONS = `
  query InspectorReactorDescriptions($correlationId: String!) {
    inspectorReactorDescriptions(correlationId: $correlationId) {
      reactorId
      blocks
    }
  }
`;

export const INSPECTOR_REACTOR_DESCRIPTION_SNAPSHOTS = `
  query InspectorReactorDescriptionSnapshots($correlationId: String!) {
    inspectorReactorDescriptionSnapshots(correlationId: $correlationId) {
      seq
      eventId
      reactorId
      blocks
    }
  }
`;

export const INSPECTOR_AGGREGATE_TIMELINE = `
  query InspectorAggregateTimeline($correlationId: String!) {
    inspectorAggregateTimeline(correlationId: $correlationId) {
      seq
      eventId
      eventType
      aggregates {
        key
        state
      }
    }
  }
`;

export const INSPECTOR_REACTOR_DEPENDENCIES = `
  query InspectorReactorDependencies {
    inspectorReactorDependencies {
      reactorId
      inputEventTypes
      outputEventTypes
    }
  }
`;

export const INSPECTOR_AGGREGATE_KEYS = `
  query InspectorAggregateKeys {
    inspectorAggregateKeys
  }
`;

export const INSPECTOR_AGGREGATE_LIFECYCLE = `
  query InspectorAggregateLifecycle($aggregateKey: String!, $limit: Int) {
    inspectorAggregateLifecycle(aggregateKey: $aggregateKey, limit: $limit) {
      seq
      eventId
      eventType
      ts
      correlationId
      aggregateKey
      state
    }
  }
`;

export const INSPECTOR_CORRELATIONS = `
  query InspectorCorrelations($search: String, $limit: Int, $cursor: String) {
    inspectorCorrelations(search: $search, limit: $limit, cursor: $cursor) {
      correlations {
        correlationId
        eventCount
        firstTs
        lastTs
        rootEventType
        hasErrors
      }
      nextCursor
    }
  }
`;

export const INSPECTOR_REACTOR_OUTCOMES = `
  query InspectorReactorOutcomes($correlationId: String!) {
    inspectorReactorOutcomes(correlationId: $correlationId) {
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
