import type { InspectorState } from "./state";
import type { InspectorEvent } from "./types";

/** Check if a seq falls within the scrubber range. */
export function inScrubberRange(seq: number, start: number | null, end: number | null): boolean {
  if (start != null && seq < start) return false;
  if (end != null && seq > end) return false;
  return true;
}

/** Derive the walkable seq list based on current context (flow, selection, or global). */
export function getScrubberSequence(state: InspectorState): number[] {
  let events: InspectorEvent[];

  if (state.flowCorrelationId) {
    events = state.flowData;
    const sel = state.flowSelection;
    if (sel) {
      if (sel.kind === "event-type") {
        events = events.filter(e => e.name === sel.name);
      } else {
        events = events.filter(e => e.reactorId === sel.reactorId);
      }
    }
  } else {
    events = state.events;
  }

  return events.map(e => e.seq).sort((a, b) => a - b);
}

/** Format a timestamp for compact display. */
export function formatTs(ts: string): string {
  return new Date(ts).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/** Compact JSON payload for inline display. */
export function compactPayload(raw: string, maxLen = 200): string {
  try {
    const obj = JSON.parse(raw);
    if (typeof obj !== "object" || obj === null) return raw.slice(0, maxLen);
    const entries = Object.entries(obj).filter(([k]) => k !== "type");
    if (entries.length === 0) return "{}";
    const parts: string[] = [];
    let len = 2;
    for (const [k, v] of entries) {
      const val =
        typeof v === "string"
          ? v.length > 60
            ? `"${v.slice(0, 57)}..."`
            : `"${v}"`
          : JSON.stringify(v);
      const part = `${k}: ${val}`;
      if (len + part.length + 2 > maxLen) {
        parts.push("...");
        break;
      }
      parts.push(part);
      len += part.length + 2;
    }
    return `{ ${parts.join(", ")} }`;
  } catch {
    return raw.slice(0, maxLen);
  }
}

/** Composite aggregate key from event fields, or null. */
export function aggregateKey(event: InspectorEvent): string | null {
  return event.aggregateType && event.aggregateId
    ? `${event.aggregateType}:${event.aggregateId}`
    : null;
}

/** Copy text to clipboard with fallback for older browsers. */
export async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    document.body.removeChild(ta);
  }
}
