import { useState } from "react";
import type { InspectorEffect } from "../types";
import { ChevronDown, ChevronRight } from "lucide-react";

function EffectRow({ effect }: { effect: InspectorEffect }) {
  const [expanded, setExpanded] = useState(false);
  const valueStr = JSON.stringify(effect.value, null, 2);
  const preview = valueStr.length > 120 ? valueStr.slice(0, 120) + "…" : valueStr;

  return (
    <div className="py-1">
      <button
        onClick={() => setExpanded((v) => !v)}
        className="flex items-start gap-1.5 w-full text-left group/effect"
      >
        <span className="mt-0.5 text-muted-foreground/40 shrink-0">
          {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
        </span>
        <span className="text-[9px] font-mono text-indigo-400/70 shrink-0">{effect.consumer}</span>
        <span className="text-[9px] text-muted-foreground/40 shrink-0">·</span>
        <span className="text-[9px] font-mono text-foreground/70 shrink-0">{effect.label}</span>
        {!expanded && (
          <span className="text-[9px] font-mono text-muted-foreground/50 truncate min-w-0">
            {preview}
          </span>
        )}
      </button>
      {expanded && (
        <pre className="mt-1 ml-4 text-[9px] font-mono text-foreground/60 whitespace-pre-wrap break-all bg-white/[0.02] rounded p-1.5 border border-border/50">
          {valueStr}
        </pre>
      )}
    </div>
  );
}

export function EffectList({ effects }: { effects: InspectorEffect[] }) {
  if (effects.length === 0) {
    return (
      <div className="py-1 text-[9px] text-muted-foreground/40 italic ml-3">No effects</div>
    );
  }
  return (
    <div className="ml-3 border-l border-indigo-500/15 pl-2">
      {effects.map((e, i) => (
        <EffectRow key={`${e.consumer}:${e.label}:${i}`} effect={e} />
      ))}
    </div>
  );
}
