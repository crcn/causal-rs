# Brainstorm: Machine Pattern (Store + Engine + Events) for Seesaw Admin UI

**Date:** 2026-03-11
**Context:** The current rootsignal admin `EventsPaneContext.tsx` bundles state, side effects, and React rendering into one 463-line context provider with ~15 `useState` calls. Every state change re-renders every consumer. Exploring the paperclip Machine pattern as a scalable architecture for the extracted `seesaw-admin-ui` package.

## Source: Paperclip Machine Pattern

From `~/Developer/crcn/paperclip/libs/js-common/src/machine/`:

- **Machine** — combines Store + Engine. Dispatch flow: `store.dispatch(event)` → `engine.handleEvent(event, currState, prevState)`
- **Store** — wraps immutable state + reducer, no side effects
- **Engine** — handles side effects, receives `(event, currState, prevState)`, can dispatch new events
- **Events** — discriminated unions (`BaseEvent<TType, TPayload>`)
- **combineEngineCreators** — composes multiple domain engines, fans out `handleEvent` to all
- **Hooks** — `useSelector` (surgical re-renders), `useDispatch`, `useInlineMachine`

The paperclip designer uses this with 7 composed engines (api, history, keyboard, shortcuts, ui, log, clipboard) and chained domain reducers.

## Proposed Domain Decomposition

```
seesaw-admin-ui/
  src/
    machine/
      events.ts          # Union: AdminMachineEvent
      state.ts           # AdminState (immutable, immer-friendly)
      reducer.ts         # Pure: (state, event) => state
      engines/
        subscription.ts  # WebSocket live events engine
        query.ts         # GraphQL query engine (pagination, causal tree, flow, logs)
        cache.ts         # Client-side event cache engine
        flow.ts          # Flow graph building engine (dagre layout computation)
      selectors.ts       # Memoized derivations
      provider.tsx       # <SeesawAdminProvider> wrapping Machine
```

## Events (Discriminated Unions)

```typescript
type AdminEvent =
  // Subscription
  | { type: "events/received"; payload: AdminEventData[] }
  | { type: "events/subscription_connected" }
  | { type: "events/subscription_error"; payload: Error }
  // Query
  | { type: "events/page_loaded"; payload: { events: AdminEventData[]; hasMore: boolean } }
  | { type: "events/causal_tree_loaded"; payload: CausalTreeData }
  | { type: "events/flow_loaded"; payload: FlowData }
  | { type: "events/logs_loaded"; payload: HandlerLog[] }
  | { type: "events/handler_descriptions_loaded"; payload: HandlerDescription[] }
  | { type: "events/handler_outcomes_loaded"; payload: HandlerOutcome[] }
  // UI
  | { type: "ui/event_selected"; payload: { seq: number } }
  | { type: "ui/flow_opened"; payload: { runId: string } }
  | { type: "ui/flow_closed" }
  | { type: "ui/filter_changed"; payload: Partial<FilterState> }
  | { type: "ui/scrubber_moved"; payload: { position: number } }
  // Flow graph (computed)
  | { type: "flow/graph_built"; payload: { nodes: Node[]; edges: Edge[] } }
  | { type: "flow/causal_chain_highlighted"; payload: { nodeIds: string[] } };
```

## State (Single Immutable Object)

```typescript
type AdminState = {
  events: AdminEventData[];
  hasMore: boolean;
  loading: boolean;

  selectedSeq: number | null;
  flowRunId: string | null;
  flowData: AdminEventData[];
  flowGraph: { nodes: Node[]; edges: Edge[] } | null;
  scrubberPosition: number | null;

  causalTree: CausalTreeData | null;
  treeSource: "dedicated" | "flow";

  filters: FilterState;
  logs: HandlerLog[];
  logsFilter: LogsFilter;

  descriptions: Map<string, HandlerDescription[]>;
  outcomes: Map<string, HandlerOutcome[]>;

  subscription: "connected" | "disconnected" | "error";
};
```

## Reducer (Pure, No Side Effects)

```typescript
const reducer = produce((draft: AdminState, event: AdminEvent) => {
  switch (event.type) {
    case "events/received":
      draft.events.unshift(...event.payload);
      break;
    case "events/page_loaded":
      draft.events.push(...event.payload.events);
      draft.hasMore = event.payload.hasMore;
      draft.loading = false;
      break;
    case "ui/event_selected":
      draft.selectedSeq = event.payload.seq;
      break;
    case "ui/flow_opened":
      draft.flowRunId = event.payload.runId;
      break;
    case "flow/graph_built":
      draft.flowGraph = event.payload;
      break;
    // ...
  }
});
```

## Engines (Side Effects, Composable)

### Subscription Engine — WebSocket live events

```typescript
const createSubscriptionEngine: EngineCreator<AdminState, AdminEvent> =
  (dispatch, getState) => {
    const client = createWSClient(getState().config.wsEndpoint);
    const sub = client.subscribe(EVENTS_SUBSCRIPTION, (data) => {
      dispatch({ type: "events/received", payload: [data.adminEventAdded] });
    });
    return {
      handleEvent: (event) => {
        // Reconnect on config change, pause when not visible, etc.
      },
      dispose: () => sub.unsubscribe(),
    };
  };
```

### Query Engine — GraphQL fetches triggered by state changes

```typescript
const createQueryEngine: EngineCreator<AdminState, AdminEvent> =
  (dispatch, getState) => ({
    handleEvent: async (event, curr, prev) => {
      // Flow opened → fetch flow data + start polling descriptions/outcomes
      if (event.type === "ui/flow_opened") {
        const data = await fetchCausalFlow(event.payload.runId);
        dispatch({ type: "events/flow_loaded", payload: data });
      }
      // Filter changed → refetch events
      if (event.type === "ui/filter_changed") {
        dispatch({ type: "events/page_loaded", payload: await fetchEvents(curr.filters) });
      }
      // Event selected → fetch causal tree
      if (event.type === "ui/event_selected") {
        const tree = await fetchCausalTree(event.payload.seq);
        dispatch({ type: "events/causal_tree_loaded", payload: tree });
      }
    },
    dispose: () => {},
  });
```

### Flow Engine — dagre layout computation (potentially off-main-thread)

```typescript
const createFlowEngine: EngineCreator<AdminState, AdminEvent> =
  (dispatch, getState) => ({
    handleEvent: (event, curr, prev) => {
      if (event.type === "events/flow_loaded" || event.type === "ui/scrubber_moved") {
        const filtered = curr.scrubberPosition
          ? curr.flowData.filter(e => e.seq <= curr.scrubberPosition)
          : curr.flowData;
        const graph = buildFlowGraph(filtered, curr.descriptions, curr.outcomes);
        dispatch({ type: "flow/graph_built", payload: graph });
      }
    },
    dispose: () => {},
  });
```

### Combined

```typescript
export const createAdminEngine = (config: SeesawAdminConfig) =>
  combineEngineCreators(
    createSubscriptionEngine,
    createQueryEngine,
    createFlowEngine,
    createCacheEngine,
  );
```

## Performance Wins Over Current Context Approach

1. **Surgical re-renders** — `useSelector(s => s.flowGraph)` only re-renders `CausalFlowPane` when the graph actually changes. Today, any state change in `EventsPaneContext` re-renders all 5 panes.

2. **Decoupled computation** — dagre layout runs in the flow engine, dispatches the result. The reducer just stores it. No layout computation in the render path.

3. **Predictable data flow** — `UI action → Event → Reducer (sync) → Engine (async) → Event → Reducer`. Easy to trace, easy to debug. The time scrubber becomes trivial: `dispatch({ type: "ui/scrubber_moved" })` → flow engine rebuilds graph → `flow/graph_built` → ReactFlow gets new nodes.

4. **Live data without re-render storms** — subscription engine appends events, but only the timeline pane (which selects `s.events`) re-renders. The flow pane doesn't care until someone dispatches a flow refresh.

5. **Testable without React** — reducers are pure functions, engines are testable with mock dispatch/getState. No need to mount components to test state logic.

## The Philosophical Mirror

The frontend architecture mirrors seesaw's own Event → Handler → Event loop:

| Seesaw (backend) | Machine (frontend) |
|---|---|
| Events | Events (discriminated unions) |
| Handlers | Engines (side effect processors) |
| Aggregates | Store (state folded from events) |
| Projections | Selectors (derived views) |
| Settlement | Engine composition (chained reactions) |

The admin tool would be built *in the idiom of the system it observes*.

## What Moves from Paperclip

The Machine infrastructure is ~200 lines total:
- `Machine` class (~30 lines)
- `Store` class (~20 lines)
- `Engine` type + `combineEngineCreators` (~30 lines)
- `BaseEvent` type (~5 lines)
- `useSelector` / `useDispatch` hooks (~40 lines)
- `ObservableMap` (~60 lines)

Options:
1. **Copy into `seesaw-admin-ui`** — simplest, no external dependency
2. **Extract as `@seesaw/machine`** — if other seesaw UI packages emerge
3. **Publish paperclip's machine as its own npm package** — if it's truly generic

Option 1 is the pragmatic choice. 200 lines isn't worth a dependency.

## Consumer Integration

Rootsignal would extend the machine with its own engines:

```typescript
import { createAdminEngine, createAdminReducer } from '@seesaw/admin-ui';
import { createInvestigationEngine } from './engines/investigation';
import { investigationReducer } from './reducers/investigation';

const rootsignalEngine = combineEngineCreators(
  createAdminEngine(config),
  createInvestigationEngine,
  createScoutRunEngine,
);

const rootsignalReducer = (state, event) =>
  [createAdminReducer, investigationReducer, scoutRunReducer]
    .reduce((s, r) => r(s, event), state);
```

## Open Questions

1. **Immer vs structuredClone** — Paperclip uses `ObservableMap` with manual immutability. Immer's `produce` is more ergonomic for complex nested state. Worth adopting for the admin UI.

2. **Web Worker for flow engine** — dagre layout on 500+ nodes can block the main thread. The engine pattern makes it trivial to move computation to a worker: the engine posts to the worker, the worker posts back, the engine dispatches `flow/graph_built`.

3. **Middleware / devtools** — The event-driven architecture makes it trivial to add logging middleware, time-travel debugging, or event replay. Should `seesaw-admin-ui` ship a devtools panel that shows its own event stream? (Meta: the admin tool debugging itself.)

4. **Selector memoization** — `useSelector` in paperclip does shallow comparison. For derived data (e.g., filtered event list), need `reselect`-style memoized selectors or `useMemo` in components.
