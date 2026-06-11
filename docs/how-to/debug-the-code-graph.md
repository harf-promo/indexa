# Debug the code graph

**Goal:** trace how files relate — who imports a module, who calls a function, what a change might
break — using Indexa's code graph. This guide also explains **how to read the results honestly**,
because the call graph is an approximation.

## The commands

```bash
# Hub files in a scope, ranked by PageRank centrality (the most-depended-on code)
indexa graph ~/code/myproject

# From the MCP tools (or the Map → Graph view in the web UI):
#   dependencies <file>   — that file's imports, defined symbols, and calls
#   who_imports <module>  — every file that imports a module path
#   who_calls <symbol>    — every file that calls a bare function/method name
#   blast_radius <symbol> — 1-hop: who calls it + what it calls
#   code_graph <scope>    — the file-to-file call graph for a directory
```

In the web UI (`indexa serve` → **Map → Graph**) the same data renders as a force-directed graph,
with node size proportional to PageRank centrality so the hubs stand out.

## How to read it — the honest caveats

Since v0.25 every call is **scope-resolved at query time** before it becomes an edge. A call to
`parse(...)` is resolved against `parse`'s definition sites in tier order:

1. **same-file** — the caller defines `parse` itself → binds locally, no repo-wide fan-out.
2. **same-dir** — a definer sits in the caller's own directory → only those are linked.
3. **import** — a definer's path matches one of the caller's imports (JS/TS relative specifiers,
   Rust `crate::`/`super::` paths, Python dotted modules — see
   [`methodology.md`](../methodology.md) for the exact forms).
4. **bare** — nothing resolved: falls back to *every* file defining the name, **labeled `(bare)`**.

The CLI and MCP surfaces tell you which is which (the web Map view and `indexa related` show
scoped edges without per-edge tier labels yet): `who_calls` groups callers by tier, `indexa graph` and
`code_graph` print `edges: N scoped (… same-dir, … import-resolved) + M bare-name` and mark bare
edges inline. **Only the bare remainder is approximate** — when every edge is scoped, there is no
caveat to apply. `--strict` / `strict: true` drops the bare tier entirely.

For the bare remainder, the old rules still hold:

- **Bare matches can over-report.** If three unrelated modules each define `parse`, an unresolved
  caller links to all three. Common names (`run`, `new`, `get`, `parse`) are the noisiest.
- **The graph can under-report** across dynamic dispatch, macros, reflection, re-exports, path
  aliases, or use-renames the import matcher doesn't follow (it is lexical matching, not a compiler).
- Treat bare edges as **"candidates to inspect," not "ground truth"**; scoped edges as "structurally
  confirmed, still worth an eyeball."

Prefer **distinctive symbol names** when using `who_calls` / `blast_radius` — the input name itself
is always matched bare (there is no definer to disambiguate against until the callers are resolved).

The full methodology, tier definitions, and per-language matcher coverage are in
[`methodology.md`](../methodology.md).

## A practical workflow

1. `indexa graph <scope>` → find the hub files (highest centrality).
2. `dependencies <hub-file>` → see what it pulls in and exposes.
3. Before changing a function, `blast_radius <function>` → the 1-hop callers to check. The
   transitive hop is resolution-confirmed where possible; eyeball anything counted under
   "bare-name" in the summary line.
