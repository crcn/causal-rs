// Compiles src/styles.css into a single, self-contained stylesheet whose every
// rule is confined to the `.causal-inspector` subtree — so the inspector ships
// its own styles and neither clobbers nor inherits the host app's CSS.
//
// Pipeline: Tailwind expands @import/@theme/@source and generates utilities,
// then postcss-prefix-selector namespaces the result under `.causal-inspector`.

import tailwindcss from "@tailwindcss/postcss";
import prefixSelector from "postcss-prefix-selector";

const SCOPE = ".causal-inspector";

// Document-level selectors have nothing to nest under inside an embedded widget,
// so collapse them onto the scope root itself (instead of `.causal-inspector html`).
const ROOT_SELECTORS = new Set(["html", "body", ":root", ":host", ":where(:root)"]);

// Tailwind emits its rules inside cascade layers. The catch: a host app's
// UNLAYERED CSS (its resets, `button {}`, etc.) beats *any* layered rule,
// regardless of specificity — so the host would still bleed into the inspector.
// Flattening the layers (preserving Tailwind's source order) makes our scoped
// `.causal-inspector …` rules compete — and win — on specificity instead.
const flattenLayers = () => ({
  postcssPlugin: "flatten-cascade-layers",
  OnceExit(root) {
    let pending = true;
    while (pending) {
      pending = false;
      root.walkAtRules("layer", (at) => {
        if (at.nodes) at.replaceWith(...at.nodes); // unwrap `@layer x { … }`
        else at.remove(); //                          drop `@layer a, b, c;`
        pending = true;
      });
    }
  },
});
flattenLayers.postcss = true;

export default {
  plugins: [
    tailwindcss(),
    prefixSelector({
      prefix: SCOPE,
      transform(prefix, selector, prefixedSelector) {
        if (ROOT_SELECTORS.has(selector.trim())) return prefix;
        return prefixedSelector;
      },
    }),
    flattenLayers(),
  ],
};
