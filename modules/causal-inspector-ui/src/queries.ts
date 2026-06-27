/** GraphQL query and subscription documents for the causal inspector API. */

const EVENT_FIELDS = `
  seq
  ts
  type
  name
  id
  causationId
  workflowId
  reactorId
  aggregateType
  aggregateId
  streamRevision
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
    $workflowId: String
    $aggregateKey: String
  ) {
    inspectorEvents(
      limit: $limit
      cursor: $cursor
      search: $search
      workflowId: $workflowId
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
  query InspectorCausalFlow($workflowId: String!) {
    inspectorCausalFlow(workflowId: $workflowId) {
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

export const INSPECTOR_REACTOR_LOGS_BY_WORKFLOW = `
  query InspectorReactorLogsByWorkflow($workflowId: String!) {
    inspectorReactorLogsByWorkflow(workflowId: $workflowId) {
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
  query InspectorReactorDescriptions($workflowId: String!) {
    inspectorReactorDescriptions(workflowId: $workflowId) {
      reactorId
      blocks
    }
  }
`;

export const INSPECTOR_REACTOR_DESCRIPTION_SNAPSHOTS = `
  query InspectorReactorDescriptionSnapshots($workflowId: String!) {
    inspectorReactorDescriptionSnapshots(workflowId: $workflowId) {
      seq
      eventId
      reactorId
      blocks
    }
  }
`;

export const INSPECTOR_AGGREGATE_TIMELINE = `
  query InspectorAggregateTimeline($workflowId: String!) {
    inspectorAggregateTimeline(workflowId: $workflowId) {
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
      workflowId
      aggregateKey
      state
    }
  }
`;

export const INSPECTOR_WORKFLOWS = `
  query InspectorWorkflows($search: String, $limit: Int, $cursor: String) {
    inspectorWorkflows(search: $search, limit: $limit, cursor: $cursor) {
      workflows {
        workflowId
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
  query InspectorReactorOutcomes($workflowId: String!) {
    inspectorReactorOutcomes(workflowId: $workflowId) {
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

export const INSPECTOR_REACTOR_ATTEMPTS = `
  query InspectorReactorAttempts($workflowId: String!) {
    inspectorReactorAttempts(workflowId: $workflowId) {
      eventId
      reactorId
      workflowId
      attempt
      status
      error
      startedAt
      completedAt
    }
  }
`;

// ── Entity-scoped inspection queries ─────────────────────────────────────

const SUBJECT_CHAIN_EVENT_FIELDS = `
  seq
  ts
  type
  name
  id
  causationId
  workflowId
  reactorId
  aggregateType
  aggregateId
  streamRevision
  summary
  payload
  sourceMode
`;

export const INSPECTOR_SUBJECT_CHAIN = `
  query InspectorSubjectChain(
    $aggregateType: String!
    $aggregateId: String!
    $mode: SubjectChainMode!
    $limit: Int
    $cursor: Int
  ) {
    inspectorSubjectChain(
      aggregateType: $aggregateType
      aggregateId: $aggregateId
      mode: $mode
      limit: $limit
      cursor: $cursor
    ) {
      events {
        ${SUBJECT_CHAIN_EVENT_FIELDS}
      }
      nextCursor
      depthCapReached
    }
  }
`;

export const INSPECTOR_EFFECTS_FOR_EVENT = `
  query InspectorEffectsForEvent($eventId: String!) {
    inspectorEffectsForEvent(eventId: $eventId) {
      consumer
      label
      value
      createdAt
    }
  }
`;

export const INSPECTOR_AGGREGATE_TYPES = `
  query InspectorAggregateTypes($search: String, $limit: Int) {
    inspectorAggregateTypes(search: $search, limit: $limit)
  }
`;

export const INSPECTOR_AGGREGATE_KEYS_BY_TYPE = `
  query InspectorAggregateKeysByType(
    $aggregateType: String!
    $search: String
    $limit: Int
    $cursor: String
  ) {
    inspectorAggregateKeysByType(
      aggregateType: $aggregateType
      search: $search
      limit: $limit
      cursor: $cursor
    ) {
      entries {
        aggregateId
        displayLabel
      }
      nextCursor
    }
  }
`;
