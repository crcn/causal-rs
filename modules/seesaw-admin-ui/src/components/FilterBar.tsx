import { useSelector, useDispatch } from "../machine";
import type { AdminState } from "../state";
import type { AdminMachineEvent } from "../events";
import { LAYER_OPTIONS, LAYER_COLORS } from "../theme";

export function FilterBar() {
  const filters = useSelector<AdminState, AdminState["filters"]>(
    (s) => s.filters
  );
  const dispatch = useDispatch<AdminMachineEvent>();

  return (
    <div className="flex flex-wrap items-center gap-2 px-3 py-2 border-b border-border bg-card/50">
      {LAYER_OPTIONS.map((layer) => {
        const active =
          filters.layers.length === 0 || filters.layers.includes(layer);
        const color = LAYER_COLORS[layer] ?? "";
        return (
          <button
            key={layer}
            onClick={() => {
              const current = filters.layers;
              const next = current.includes(layer)
                ? current.filter((l) => l !== layer)
                : [...current, layer];
              dispatch({ type: "ui/filter_changed", payload: { layers: next } });
            }}
            className={`px-2 py-0.5 rounded text-xs font-medium transition-opacity ${color} ${
              active ? "opacity-100" : "opacity-30"
            }`}
          >
            {layer}
          </button>
        );
      })}

      <span className="w-px h-4 bg-border" />

      <input
        type="text"
        placeholder="search events..."
        value={filters.search}
        onChange={(e) =>
          dispatch({
            type: "ui/filter_changed",
            payload: { search: e.target.value },
          })
        }
        className="px-2 py-1 text-xs rounded bg-background border border-border text-foreground placeholder:text-muted-foreground w-64"
      />

      <span className="w-px h-4 bg-border" />

      <label className="text-xs text-muted-foreground">From</label>
      <input
        type="date"
        value={filters.from ?? ""}
        onChange={(e) =>
          dispatch({
            type: "ui/filter_changed",
            payload: { from: e.target.value || null },
          })
        }
        className="px-2 py-1 text-xs rounded bg-background border border-border text-foreground w-32"
      />
      <label className="text-xs text-muted-foreground">To</label>
      <input
        type="date"
        value={filters.to ?? ""}
        onChange={(e) =>
          dispatch({
            type: "ui/filter_changed",
            payload: { to: e.target.value || null },
          })
        }
        className="px-2 py-1 text-xs rounded bg-background border border-border text-foreground w-32"
      />

      {filters.runId && (
        <>
          <span className="w-px h-4 bg-border" />
          <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded bg-purple-500/20 text-purple-400 text-[10px] font-mono">
            run: {filters.runId.slice(0, 8)}...
            <button
              onClick={() =>
                dispatch({
                  type: "ui/filter_changed",
                  payload: { runId: null },
                })
              }
              className="hover:text-foreground"
            >
              x
            </button>
          </span>
        </>
      )}
    </div>
  );
}
