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
    <div className="flex flex-wrap items-center gap-2 px-3 py-2 border-b border-border" style={{ background: "rgba(15, 15, 20, 0.6)", backdropFilter: "blur(8px)" }}>
      <div className="relative flex items-center">
        <Search size={12} className="absolute left-2.5 text-muted-foreground pointer-events-none" />
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
          className="pl-7 pr-2 py-1.5 text-xs rounded-md bg-background/50 border border-border text-foreground placeholder:text-muted-foreground w-64 focus:outline-none focus:ring-1 focus:ring-indigo-500/40 focus:border-indigo-500/30 transition-all"
        />
      </div>

      {filters.correlationId && (
        <>
          <span className="w-px h-4 bg-border" />
          <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-purple-500/10 border border-purple-500/20 text-purple-400 text-[10px] font-mono">
            {filters.correlationId.slice(0, 8)}
            <button
              onClick={() =>
                dispatch({
                  type: "ui/filter_changed",
                  payload: { correlationId: null },
                })
              }
              className="hover:text-foreground transition-colors"
            >
              <X size={10} />
            </button>
          </span>
        </>
      )}

      {filters.aggregateKey && (
        <>
          <span className="w-px h-4 bg-border" />
          <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-teal-500/10 border border-teal-500/20 text-teal-400 text-[10px] font-mono">
            {filters.aggregateKey.split(":")[0]}:{filters.aggregateKey.split(":").slice(1).join(":").slice(0, 8)}
            <button
              onClick={() =>
                dispatch({
                  type: "ui/filter_changed",
                  payload: { aggregateKey: null },
                })
              }
              className="hover:text-foreground transition-colors"
            >
              <X size={10} />
            </button>
          </span>
        </>
      )}
    </div>
  );
}
