/** Deterministic hue from event name — consistent colors across renders. */
export function eventHue(name: string): number {
  let hash = 0;
  for (const ch of name) hash = (hash * 31 + ch.charCodeAt(0)) | 0;
  return ((hash % 360) + 360) % 360;
}

/** Background color blended onto dark background — more saturated. */
export function eventBg(name: string): string {
  const h = eventHue(name);
  const [r, g, b] = hslToRgb(h, 75, 50);
  const br = 10, bg = 10, bb = 15; // #0a0a0f
  const a = 0.15;
  return `rgb(${Math.round(br + (r - br) * a)}, ${Math.round(bg + (g - bg) * a)}, ${Math.round(bb + (b - bb) * a)})`;
}

export function eventBorder(name: string): string {
  return `hsla(${eventHue(name)}, 65%, 55%, 0.5)`;
}

export function eventTextColor(name: string): string {
  return `hsl(${eventHue(name)}, 70%, 70%)`;
}

function hslToRgb(h: number, s: number, l: number): [number, number, number] {
  s /= 100;
  l /= 100;
  const k = (n: number) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => l - a * Math.max(-1, Math.min(k(n) - 3, 9 - k(n), 1));
  return [Math.round(f(0) * 255), Math.round(f(8) * 255), Math.round(f(4) * 255)];
}

export const LOG_LEVEL_COLORS: Record<string, string> = {
  debug: "bg-zinc-600/20 text-zinc-400",
  info: "bg-indigo-500/15 text-indigo-400",
  warn: "bg-amber-500/15 text-amber-400",
  error: "bg-red-500/15 text-red-400",
};
