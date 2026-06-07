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

The call graph is **bare-name matched**: a call to `parse(...)` links to *every* file that defines a
symbol named `parse`, case-sensitive, **1 hop**, across the 7 supported languages. It does **not**
resolve imports, namespaces, types, or overloads. So:

- **`who_calls parse` can over-report.** If three unrelated modules each define `parse`, a caller of
  any one of them links to all three. Common names (`run`, `new`, `get`, `parse`) are the noisiest.
- **It can under-report** across dynamic dispatch, macros, reflection, or re-exports the parser
  doesn't follow.
- Treat the output as **"candidates to inspect," not "ground truth."** It's a fast way to find where
  to look, not a substitute for the compiler.

Prefer **distinctive symbol names** when using `who_calls` / `blast_radius` — the rarer the name,
the more trustworthy the edge.

The full methodology and per-language coverage are in [`methodology.md`](../methodology.md).

## A practical workflow

1. `indexa graph <scope>` → find the hub files (highest centrality).
2. `dependencies <hub-file>` → see what it pulls in and exposes.
3. Before changing a function, `blast_radius <function>` → the 1-hop callers to check — then
   eyeball each, because of the bare-name caveat above.
