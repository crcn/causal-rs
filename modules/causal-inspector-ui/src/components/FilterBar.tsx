import { useSelector, useDispatch } from "../machine";
import type { InspectorState } from "../state";
import type { InspectorMachineEvent } from "../events";
import { Search, X } from "lucide-react";

export function FilterBar() {
  const filters = useSelector<InspectorState, InspectorState["filters"]>(
    (s) => s.filters
  );
  const dispatch = useDispatch<InspectorMachineEvent>();

  return (
    <div className="flex flex-wrap items-center gap-2 px-3 py-2 border-b border-border bg-card/50">
      <div className="relative flex items-center">
        <Search size={12} className="absolute left-2 text-muted-foreground pointer-events-none" />
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
          className="pl-7 pr-2 py-1 text-xs rounded bg-background border border-border text-foreground placeholder:text-muted-foreground w-64"
        />
      </div>

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

      {filters.correlationId && (
        <>
          <span className="w-px h-4 bg-border" />
          <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded bg-purple-500/20 text-purple-400 text-[10px] font-mono">
            correlation: {filters.correlationId.slice(0, 8)}...
            <button
              onClick={() =>
                dispatch({
                  type: "ui/filter_changed",
                  payload: { correlationId: null },
                })
              }
              className="hover:text-foreground"
            >
              <X size={10} />
            </button>
          </span>
        </>
      )}
    </div>
  );
}
