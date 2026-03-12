import { useMemo, useEffect, useRef, useCallback } from "react";
import {
  Layout,
  Model,
  Actions,
  DockLocation,
  type IJsonModel,
  type Action,
  type TabNode,
  type TabSetNode,
  type ITabSetRenderValues,
} from "flexlayout-react";
import type { BorderNode } from "flexlayout-react";
import {
  CausalInspectorProvider,
  createInspectorEngine,
  useSelector,
  useDispatch,
  TimelinePane,
  CausalTreePane,
  CausalFlowPane,
  LogsPane,
  AggregateTimelinePane,
  WaterfallPane,
  CorrelationExplorerPane,
  ReactorDependencyMapPane,
  type InspectorState,
  type InspectorMachineEvent,
  type PaneLayout,
} from "@causal/inspector-ui";
import { Plus } from "lucide-react";
import { createTransport } from "./transport";

const origin = window.location.origin;
const graphqlUrl = `${origin}/`;
const wsUrl = `${origin.replace(/^http/, "ws")}/ws`;

// ── Layout persistence ─────────────────────────────────────────

const STORAGE_KEY = "inspector-pane-layout-v2";

const DEFAULT_LAYOUT: IJsonModel = {
  global: {
    tabEnableClose: true,
    tabSetEnableMaximize: true,
    tabSetEnableTabStrip: true,
    splitterSize: 6,
    splitterExtra: 4,
    enableEdgeDock: false,
  },
  layout: {
    type: "row",
    children: [
      {
        type: "tabset",
        weight: 55,
        children: [
          { type: "tab", name: "Timeline", component: "timeline" },
        ],
      },
      {
        type: "tabset",
        weight: 45,
        children: [
          { type: "tab", name: "Causal Tree", component: "causal-tree" },
        ],
      },
    ],
  },
};

/** Load and validate saved layout. Returns null if missing or corrupt. */
function loadSavedLayout(): PaneLayout | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const json = JSON.parse(raw) as IJsonModel;
    // Validate by actually constructing a Model — if the JSON is
    // corrupt or from an incompatible version this will throw.
    Model.fromJson(json);
    return json as unknown as PaneLayout;
  } catch {
    localStorage.removeItem(STORAGE_KEY);
    return null;
  }
}

// ── Pane registry ──────────────────────────────────────────────

const PANE_REGISTRY = [
  { name: "Timeline", component: "timeline", render: () => <TimelinePane /> },
  { name: "Causal Tree", component: "causal-tree", render: () => <CausalTreePane /> },
  { name: "Flow", component: "causal-flow", render: () => <CausalFlowPane /> },
  { name: "Logs", component: "logs", render: () => <LogsPane /> },
{ name: "State Timeline", component: "state-timeline", render: () => <AggregateTimelinePane /> },
  { name: "Waterfall", component: "waterfall", render: () => <WaterfallPane /> },
  { name: "Correlations", component: "correlations", render: () => <CorrelationExplorerPane /> },
  { name: "Reactor Map", component: "reactor-map", render: () => <ReactorDependencyMapPane /> },
] as const;

// ── Helpers ────────────────────────────────────────────────────

function findTab(model: Model, component: string): TabNode | null {
  let found: TabNode | null = null;
  model.visitNodes((node) => {
    if (node.getType() === "tab" && (node as TabNode).getComponent() === component) {
      found = node as TabNode;
    }
  });
  return found;
}

// ── InspectorLayout ────────────────────────────────────────────

function InspectorLayout() {
  const paneLayout = useSelector<InspectorState, PaneLayout | null>((s) => s.paneLayout);
  const flowCorrelationId = useSelector<InspectorState, string | null>((s) => s.flowCorrelationId);
  const selectedSeq = useSelector<InspectorState, number | null>((s) => s.selectedSeq);
  const dispatch = useDispatch<InspectorMachineEvent>();

  // Build Model once from state (which was seeded from localStorage via initialState).
  const modelRef = useRef<Model>(null!);
  if (!modelRef.current) {
    const json = (paneLayout as IJsonModel | null) ?? DEFAULT_LAYOUT;
    try {
      modelRef.current = Model.fromJson(json);
    } catch {
      modelRef.current = Model.fromJson(DEFAULT_LAYOUT);
    }
  }
  const layoutRef = useRef<Layout>(null);

  const addTab = useCallback((component: string, name: string) => {
    const model = modelRef.current;
    const existing = findTab(model, component);
    if (existing) {
      model.doAction(Actions.selectTab(existing.getId()));
      return;
    }
    const target = model.getActiveTabset()?.getId()
      ?? model.getRoot().getChildren()[0]?.getId()
      ?? "";
    model.doAction(Actions.addNode({ type: "tab", component, name }, target, DockLocation.CENTER, -1));
  }, []);

  useEffect(() => {
    if (selectedSeq != null) addTab("causal-tree", "Causal Tree");
  }, [selectedSeq, addTab]);

  useEffect(() => {
    if (flowCorrelationId) addTab("causal-flow", "Flow");
  }, [flowCorrelationId, addTab]);

  const factory = useCallback((node: TabNode) => {
    const component = node.getComponent();
    const pane = PANE_REGISTRY.find((p) => p.component === component);
    if (!pane) return <div className="p-4 text-sm text-muted-foreground">Unknown pane: {component}</div>;
    return <div className="h-full overflow-hidden">{pane.render()}</div>;
  }, []);

  const onModelChange = useCallback((model: Model, action: Action) => {
    dispatch({ type: "ui/layout_changed", payload: model.toJson() as PaneLayout });

    if (action.type === Actions.DELETE_TAB) {
      if (!findTab(model, "logs")) {
        dispatch({ type: "ui/logs_filter_changed", payload: { eventId: null, reactorId: null, correlationId: null, scope: "reactor" } });
      }
      if (!findTab(model, "causal-flow")) {
        dispatch({ type: "ui/flow_closed" });
      }
    }
  }, [dispatch]);

  const onRenderTabSet = useCallback(
    (_node: TabSetNode | BorderNode, renderValues: ITabSetRenderValues) => {
      renderValues.stickyButtons.push(
        <button
          key="add-pane"
          className="flexlayout__tab_toolbar_button"
          title="Add pane"
          onClick={(e) => {
            const btn = e.currentTarget as HTMLElement;
            const rect = btn.getBoundingClientRect();
            const menu = document.createElement("div");
            menu.style.cssText = `position:fixed;z-index:9999;background:#18181b;border:1px solid #27272a;border-radius:6px;padding:4px;box-shadow:0 4px 12px rgba(0,0,0,0.5);top:${rect.bottom + 4}px;right:${window.innerWidth - rect.right}px;`;

            for (const pane of PANE_REGISTRY) {
              const item = document.createElement("button");
              item.textContent = pane.name;
              item.style.cssText = "display:block;width:100%;text-align:left;padding:6px 12px;font-size:12px;color:#fafafa;background:transparent;border:none;border-radius:4px;cursor:pointer;";
              item.onmouseenter = () => { item.style.background = "#27272a"; };
              item.onmouseleave = () => { item.style.background = "transparent"; };
              item.onclick = () => { addTab(pane.component, pane.name); menu.remove(); };
              menu.appendChild(item);
            }

            document.body.appendChild(menu);
            const close = (e: MouseEvent) => {
              if (!menu.contains(e.target as Node)) { menu.remove(); document.removeEventListener("mousedown", close); }
            };
            setTimeout(() => document.addEventListener("mousedown", close), 0);
          }}
        >
          <Plus size={14} />
        </button>,
      );
    },
    [addTab],
  );

  return (
    <div className="h-screen w-screen">
      <Layout
        ref={layoutRef}
        model={modelRef.current}
        factory={factory}
        onModelChange={onModelChange}
        onRenderTabSet={onRenderTabSet}
      />
    </div>
  );
}

// ── App ──────────────────────────────────────────────────────────

const savedLayout = loadSavedLayout();

export default function App() {
  const transport = useMemo(() => createTransport(graphqlUrl, wsUrl), []);

  const createEngine = useMemo(
    () =>
      createInspectorEngine(transport, {
        saveLayout: (layout) => {
          try { localStorage.setItem(STORAGE_KEY, JSON.stringify(layout)); } catch {}
        },
      }),
    [transport],
  );

  return (
    <CausalInspectorProvider
      createEngine={createEngine}
      initialState={savedLayout ? { paneLayout: savedLayout } : undefined}
    >
      <InspectorLayout />
    </CausalInspectorProvider>
  );
}
