# Web UI assets

The Indexa web UI is **pure vanilla JS + SVG — zero frontend libraries, no bundler, no
build step.** The files here are concatenated at compile time into the binary via
`include_str!` / `concat!` in [`crates/web/src/lib.rs`](../../src/lib.rs).

## Layout

- `index.html` — the single page, embedded as `UI_HTML`.
- `js/NN-name.js` — behaviour, concatenated **in numeric order** into `UI_JS`.
- `css/NN-name.css` — styles, concatenated **in numeric order** into `UI_CSS`.

The numeric prefix **is** the load order.

## The one rule: append to the concat list or it is dead

A new `js/NN-name.js` or `css/NN-name.css` file does **nothing** until you add a matching
`include_str!(...)` line to the `UI_JS` / `UI_CSS` `concat!` in `crates/web/src/lib.rs`.
There is no glob, no directory scan — a file that isn't in the list is silently ignored.
Keep the `concat!` entries in the same numeric order as the filenames.

## Why order matters

All JS files concatenate into **one script sharing a single global scope** — there are no
ES modules. So:

- A `function foo(){}` declaration is hoisted and callable from any file regardless of
  order, but top-level `const`/`let` and any code that *runs* at load time executes in
  concat order. If file `B` runs code that reads a `const` defined in file `A`, `A` must
  come first.
- CSS follows the cascade: a later file overrides an earlier one. The numbering is used
  deliberately for this — e.g. `18-graph-explore.css` re-asserts rules after `11-graph.css`
  so its overrides win.

## Conventions

- Build SVG with `document.createElementNS(...)`, not `innerHTML`, so attributes like
  `viewBox` and namespaced elements work.
- The bundle contains emoji and other multi-byte glyphs — search it with `grep -a` (treat
  as text), or `grep` may skip "binary" matches.
- Keep it dependency-free. Anything that would pull in a framework or a build pipeline
  does not belong here.

See `docs/architecture.md` ("Add a web endpoint") for the server side.
