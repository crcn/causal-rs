import { useMemo, useRef, useCallback, useEffect } from "react";
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
import { createClient } from "graphql-ws";
import { Plus } from "lucide-react";

import { CausalInspectorProvider } from "./context";
import { createInspectorEngine } from "./engines";
import type { InspectorTransport, StorageTransport } from "./engines";
import { useSelector, useDispatch } from "./machine";
import type { InspectorState } from "./state";
import type { InspectorMachineEvent } from "./events";
import type { PaneLayout } from "./types";

import { TimelinePane } from "./panes/TimelinePane";
import { CausalTreePane } from "./panes/CausalTreePane";
import { CausalFlowPane } from "./panes/CausalFlowPane";
import { LogsPane } from "./panes/LogsPane";
import { AggregateTimelinePane } from "./panes/AggregateTimelinePane";
import { WaterfallPane } from "./panes/WaterfallPane";
import { CorrelationExplorerPane } from "./panes/CorrelationExplorerPane";
import { GlobalScrubber } from "./components/GlobalScrubber";

import "./CausalInspector.css";

// ── Types ─────────────────────────────────────────────────────

export type CausalInspectorProps = {
  /** GraphQL endpoint URL (relative or absolute). Queries POST here, WS connects to {endpoint}/ws */
  endpoint: string;
  /** Extra fetch options. Defaults to { credentials: "include" } */
  fetchOptions?: {
    credentials?: RequestCredentials;
    headers?: Record<string, string>;
  };
  /** CSS class for the container */
  className?: string;
};

// ── Transport ─────────────────────────────────────────────────

function createTransport(
  endpoint: string,
  fetchOptions?: CausalInspectorProps["fetchOptions"],
): InspectorTransport {
  const url = new URL(endpoint, window.location.origin);
  const httpUrl = url.toString();
  const wsUrl = httpUrl.replace(/^http/, "ws") + "/ws";

  const wsClient = createClient({ url: wsUrl });

  return {
    async query<T>(
      query: string,
      variables?: Record<string, unknown>,
    ): Promise<T> {
      const res = await fetch(httpUrl, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          ...fetchOptions?.headers,
        },
        credentials: fetchOptions?.credentials ?? "include",
        body: JSON.stringify({ query, variables }),
      });
      const json = await res.json();
      if (json.errors) {
        throw new Error(
          json.errors
            .map((e: { message: string }) => e.message)
            .join(", "),
        );
      }
      return json.data as T;
    },

    subscribe(
      query: string,
      variables: Record<string, unknown>,
      onData: (data: unknown) => void,
      onError?: (error: unknown) => void,
    ): () => void {
      let disposed = false;

      const unsubscribe = wsClient.subscribe(
        { query, variables },
        {
          next(value) {
            if (!disposed && value.data) onData(value.data);
          },
          error(err) {
            if (!disposed) onError?.(err);
          },
          complete() {},
        },
      );

      return () => {
        disposed = true;
        unsubscribe();
      };
    },
  };
}

// ── Layout persistence ────────────────────────────────────────

const STORAGE_KEY = "causal-inspector-layout";

function loadSavedLayout(): PaneLayout | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const json = JSON.parse(raw) as IJsonModel;
    Model.fromJson(json); // validate
    return json as unknown as PaneLayout;
  } catch {
    localStorage.removeItem(STORAGE_KEY);
    return null;
  }
}

const storage: StorageTransport = {
  saveLayout: (layout) => {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(layout));
    } catch {}
  },
};

// ── Default layout ────────────────────────────────────────────

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

// ── Pane registry ─────────────────────────────────────────────

const PANE_REGISTRY = [
  { name: "Timeline", component: "timeline", render: () => <TimelinePane /> },
  {
    name: "Causal Tree",
    component: "causal-tree",
    render: () => <CausalTreePane />,
  },
  {
    name: "Flow",
    component: "causal-flow",
    render: () => <CausalFlowPane />,
  },
  { name: "Logs", component: "logs", render: () => <LogsPane /> },
  {
    name: "State Timeline",
    component: "state-timeline",
    render: () => <AggregateTimelinePane />,
  },
  {
    name: "Waterfall",
    component: "waterfall",
    render: () => <WaterfallPane />,
  },
  {
    name: "Correlations",
    component: "correlations",
    render: () => <CorrelationExplorerPane />,
  },
] as const;

// ── Helpers ───────────────────────────────────────────────────

function findTab(model: Model, component: string): TabNode | null {
  let found: TabNode | null = null;
  model.visitNodes((node) => {
    if (
      node.getType() === "tab" &&
      (node as TabNode).getComponent() === component
    ) {
      found = node as TabNode;
    }
  });
  return found;
}

// ── InspectorLayout (inner component) ─────────────────────────

function InspectorLayout() {
  const paneLayout = useSelector<InspectorState, PaneLayout | null>(
    (s) => s.paneLayout,
  );
  const flowCorrelationId = useSelector<InspectorState, string | null>(
    (s) => s.flowCorrelationId,
  );
  const selectedSeq = useSelector<InspectorState, number | null>(
    (s) => s.selectedSeq,
  );
  const dispatch = useDispatch<InspectorMachineEvent>();

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
    const target =
      model.getActiveTabset()?.getId() ??
      model.getRoot().getChildren()[0]?.getId() ??
      "";
    model.doAction(
      Actions.addNode(
        { type: "tab", component, name },
        target,
        DockLocation.CENTER,
        -1,
      ),
    );
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
    if (!pane)
      return (
        <div style={{ padding: 16, fontSize: 12, color: "#9090a0" }}>
          Unknown pane: {component}
        </div>
      );
    return (
      <div style={{ height: "100%", overflow: "hidden" }}>{pane.render()}</div>
    );
  }, []);

  const onModelChange = useCallback(
    (model: Model, action: Action) => {
      dispatch({
        type: "ui/layout_changed",
        payload: model.toJson() as unknown as PaneLayout,
      });

      if (action.type === Actions.DELETE_TAB) {
        if (!findTab(model, "causal-flow")) {
          dispatch({ type: "ui/flow_closed" });
        }
      }
    },
    [dispatch],
  );

  const onRenderTabSet = useCallback(
    (
      _node: TabSetNode | BorderNode,
      renderValues: ITabSetRenderValues,
    ) => {
      renderValues.stickyButtons.push(
        <button
          key="add-pane"
          className="flexlayout__tab_toolbar_button"
          title="Add pane"
          onClick={(e) => {
            const btn = e.currentTarget as HTMLElement;
            const rect = btn.getBoundingClientRect();
            const menu = document.createElement("div");
            menu.style.cssText = `position:fixed;z-index:9999;background:rgba(17,17,22,0.95);backdrop-filter:blur(12px);border:1px solid rgba(255,255,255,0.06);border-radius:10px;padding:4px;box-shadow:0 8px 32px rgba(0,0,0,0.5);top:${rect.bottom + 6}px;right:${window.innerWidth - rect.right}px;`;

            for (const pane of PANE_REGISTRY) {
              const item = document.createElement("button");
              item.textContent = pane.name;
              item.style.cssText =
                "display:block;width:100%;text-align:left;padding:7px 14px;font-size:11px;color:rgba(240,240,245,0.7);background:transparent;border:none;border-radius:6px;cursor:pointer;transition:all 100ms;letter-spacing:0.02em;";
              item.onmouseenter = () => {
                item.style.background = "rgba(255,255,255,0.04)";
                item.style.color = "rgba(240,240,245,0.95)";
              };
              item.onmouseleave = () => {
                item.style.background = "transparent";
                item.style.color = "rgba(240,240,245,0.7)";
              };
              item.onclick = () => {
                addTab(pane.component, pane.name);
                menu.remove();
              };
              menu.appendChild(item);
            }

            document.body.appendChild(menu);
            const close = (ev: MouseEvent) => {
              if (!menu.contains(ev.target as Node)) {
                menu.remove();
                document.removeEventListener("mousedown", close);
              }
            };
            setTimeout(
              () => document.addEventListener("mousedown", close),
              0,
            );
          }}
        >
          <Plus size={14} />
        </button>,
      );
    },
    [addTab],
  );

  return (
    <>
      <div style={{ flex: 1, minHeight: 0 }}>
        <Layout
          ref={layoutRef}
          model={modelRef.current}
          factory={factory}
          onModelChange={onModelChange}
          onRenderTabSet={onRenderTabSet}
        />
      </div>
      <GlobalScrubber />
    </>
  );
}

// ── CausalInspector (public API) ──────────────────────────────

const savedLayout = loadSavedLayout();

export function CausalInspector({
  endpoint,
  fetchOptions,
  className,
}: CausalInspectorProps) {
  const transport = useMemo(
    () => createTransport(endpoint, fetchOptions),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [endpoint],
  );

  const createEngine = useMemo(
    () => createInspectorEngine(transport, storage),
    [transport],
  );

  return (
    <div className={`causal-inspector${className ? ` ${className}` : ""}`}>
      <CausalInspectorProvider
        createEngine={createEngine}
        initialState={savedLayout ? { paneLayout: savedLayout } : undefined}
      >
        <InspectorLayout />
      </CausalInspectorProvider>
    </div>
  );
}
