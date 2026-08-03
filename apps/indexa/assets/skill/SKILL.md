---
name: indexa
description: How to query the Indexa MCP server efficiently — which tool for which question, progressive disclosure, and the retrieval knobs worth knowing about.
---

# Using Indexa well

Indexa is a local context engine: a hierarchically-summarized index of this project, served
over MCP so you don't have to re-read files from scratch. Prefer it over `Grep`/`Read` when
the question is about understanding, not editing, the codebase.

## Which tool for which question

| You want to know... | Call |
|---|---|
| "What's in this folder?" | `browse_tree` |
| "What does this file/module do?" (cheap) | `get_summary` with `tier: "l0"` (one-line abstract) |
| "What does this file do?" (full) | `get_summary` with `tier: "l1"`, then `tier: "l2"` for raw content |
| A free-text question, grounded in an answer | `ask` |
| Same, but you'd rather write the answer yourself | `ask` with `synthesize: false` — returns the retrieved slice, no local-model call, no token cost on your side |
| A specific file's exact raw text | `read_file` |
| Keyword/semantic search across content | `search` |
| A file's imports/defines/calls (+ heritage) | `dependencies` |
| Who calls a function/method by name | `who_calls` |
| "What breaks if I change X?" | `blast_radius` (set `grouped: true` for a WILL BREAK / LIKELY AFFECTED / MAY NEED TESTING breakdown) |
| "What did I just touch and what does it break?" | `changed_impact` (diffs git, maps to symbols, runs blast radius) |
| "How does A reach B?" | `trace_path` |
| A 360° view of one symbol (defs + callers + heritage) | `symbol_context` — replaces `who_calls` + `dependencies` + a manual heritage check |
| Files worth reading alongside this one | `related_files` |

## Progressive disclosure — scan cheap, drill selectively

Don't call `read_file` or `get_summary(tier: "l2")` on every file up front. The index is
built so a cheap first pass tells you where to look:

1. `browse_tree` or `search` to find candidates.
2. `get_summary(tier: "l0")` on each candidate — one line each, cheap to read many of.
3. Only `tier: "l1"`/`tier: "l2"` (or `read_file`) on the 1-3 files that actually matter.

For a broad "what does this project do" question, `ask` with a project-level query already
does this internally (it retrieves, ranks, and only feeds the LLM the top slice) — you don't
need to manually walk the tree first.

## `ask` synthesizes locally — use `synthesize: false` if you're a strong model

`ask` answers using Indexa's own local model (e.g. `gemma3:12b` via Ollama), not you. If
you're a capable model reading this, calling `ask` with `synthesize: false` gets you the same
retrieved, ranked context slice *without* a local-model round-trip — usually better quality
(your own reasoning) and no extra latency cost. Reserve `synthesize: true` (the default) for
when you want Indexa to hand you a ready-made answer rather than raw material.

## Context Packs — named, reusable bundles

If you're going to reference the same set of files repeatedly in a session (a feature area, a
bug investigation), build a Context Pack once: `create_pack` → `add_pack_paths` → `export_pack`
(or `search_pack` to query within it). Cheaper than re-discovering the same files via `search`
every time, and exportable as XML/Markdown for pasting elsewhere.

## The Decision Ledger — answer on the user's behalf when asked

`list_open_decisions` surfaces judgment calls Indexa couldn't make automatically (ambiguous
symbol names, likely duplicates, stale-looking directories). If one is relevant to what you're
doing, relay it to the user or use `answer_decision` with your best judgment — don't leave it
open if you already have the context to resolve it.

## Predicate grammar in search (if enabled)

`search`/`ask` accept inline predicates in the free-text query — `path:src/auth`,
`ext:md`, `type:python` (a curated named set expanding to multiple extensions, e.g.
`.py`/`.pyi` — see `docs/config.md` for the full list) — to scope without a separate param.
This is config-gated (`[retrieval] query_predicates`) and off by default; if predicates don't
seem to narrow results, they may not be enabled for this index — fall back to plain keywords or
`search`'s explicit scope handling instead of assuming the syntax works.

## Staleness

`ask`/`search` results are annotated when a cited file has changed on disk since it was last
indexed (look for a `(stale: ...)` marker or a footer note). Treat a stale citation as
"probably still relevant, but re-check with `read_file` before relying on exact details."
