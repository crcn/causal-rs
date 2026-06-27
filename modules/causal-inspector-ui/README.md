# causal-inspector

React UI for inspecting causal-rs event streams, workflows, and reactor logs.

## Install

```sh
npm install causal-inspector
```

Peer deps (provide the ones you use): `react`, `react-dom`, and — for the flow /
layout panes — `@xyflow/react`, `@dagrejs/dagre`, `flexlayout-react`.

## Embedding

The inspector ships **self-contained, isolated styles** — it does not require
Tailwind (or anything else) in the host app. Two imports:

```tsx
import { CausalInspector } from "causal-inspector";
import "causal-inspector/styles.css"; // utilities + tokens + flow/layout CSS, all scoped
```

That's it. Render it anywhere:

```tsx
<CausalInspector transport={transport} />
```

## Style isolation

All shipped CSS is confined to the `.causal-inspector` root the component renders:

- **Nothing leaks out** — every selector in `dist/causal-inspector.css` is namespaced
  under `.causal-inspector`, so the inspector can't restyle the host app. The
  third-party flow/layout CSS (`@xyflow/react`, `flexlayout-react`) is bundled and
  scoped too, instead of polluting the global stylesheet.
- **The host doesn't leak in** — the bundle carries its own preflight reset and
  design tokens (defined on `.causal-inspector`), and is emitted **without cascade
  layers** so its scoped rules win on specificity over a host's global resets,
  element styles, and `body`/reset CSS.

**Known limit:** scope-based isolation can't override a host rule that targets the
*same* Tailwind class name with `!important` (only possible if the host also runs
Tailwind), nor a pathological `* { font-family }`. If you embed inside a Tailwind
host, build with a class prefix (see below) to eliminate name collisions entirely.

### Re-theming

Override the design tokens on the root from the host:

```css
.causal-inspector {
  --color-background: #101014;
  --color-foreground: #e7e7ef;
  /* …any of the --color-* / --radius tokens… */
}
```

## Build

```sh
npm run build       # tsc → dist, copy scoped overrides, compile the CSS bundle
npm run build:css   # just recompile dist/causal-inspector.css
```

The CSS bundle is produced by `postcss.config.mjs`: Tailwind v4 compiles
`src/styles.css` (scanning `src/**/*.{ts,tsx}` for the utilities actually used),
then every selector is scoped under `.causal-inspector` and the cascade layers are
flattened. To harden for a Tailwind host, add a Tailwind `prefix(...)` in
`src/styles.css` and the matching prefix to component class names.
