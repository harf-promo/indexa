# Competitive landscape

Where Indexa sits, who's nearby, and what makes it defensible. Honest, with the gaps named. Snapshot:
May 2026.

## The one-line position

Indexa is **the local context engine for AI** — it indexes your disk or repo once, builds a persistent
hierarchical context graph, and serves any AI tool (cloud or local) a small relevant slice on demand,
over CLI, a web UI, and MCP.

## The uncontested intersection

No competitor occupies all six of these at once. Indexa does:

1. **Local-first** — offline, private, free; your data never leaves the machine unless you point it at a cloud model.
2. **Whole-disk *and* code** — documents, code, images, audio, video; not repo-only, not docs-only.
3. **Persistent index + retrieval** — a queryable store with hybrid search, not a one-shot context dump.
4. **Three interfaces** — CLI, local web workspace, and a native **MCP** server for agents.
5. **Resource-aware** — a memory watchdog that won't freeze the machine running local models.
6. **Dual-audience** — saves *cloud* tools their paid tokens **and** gives *local* models context they can't hold.

Most tools nail one or two. The combination is the moat.

## Landscape matrix

| Tool | Local | Scope | Persistent index | Interfaces | Niche |
|---|---|---|---|---|---|
| **Indexa** | ✅ | whole-disk + code | ✅ hybrid retrieval | CLI · web · MCP | the engine |
| Repomix / gitingest / code2prompt | ✅ | one repo | ❌ one-shot pack | CLI (some MCP) | repo→prompt |
| AnythingLLM / Khoj / Onyx | ✅ | manual docs | ✅ | desktop/web (MCP emerging) | local doc-chat |
| Continue.dev | ✅/cloud | repo (@codebase) | partial | IDE + MCP | coding assistant |
| Cursor / Cody | cloud | repo | cloud index | IDE | coding assistant |
| graphify | calls Claude cloud | folder + media | regenerated per run | skill + web + MCP | knowledge **graph** |
| Understand-Anything | calls Claude cloud | code | JSON per run | plugin + web dashboard | codebase **graph** |
| MS GraphRAG / potpie / blarify | mixed (Neo4j/cloud) | docs / code | graph DB | library / service | GraphRAG |
| Spotlight / Everything / Recoll | ✅ | whole-disk | ✅ filename/FTS | OS / app | filename search |

## Closest threats — and the difference

- **Repomix / gitingest / code2prompt** — popular repo→LLM packers; some have MCP. But they're one-shot:
  no persistent index, no retrieval, no relevance slice, no whole-disk. *Indexa adds persistence,
  retrieval, and a ranked slice instead of dumping the whole repo.*
- **AnythingLLM / Khoj / Onyx** — local "second brain" / doc-chat. But ingest is **manual** (drop folders
  in), they're heavier (Postgres/Docker), and they have no code intelligence. *Indexa points at any folder,
  is a single binary, and treats code as a first-class citizen.*
- **Continue.dev / Cursor / Cody** — strong codebase context **inside the IDE**; Cursor/Cody are cloud.
  *Indexa is a standalone, disk-wide engine that **feeds** these tools (and Claude Code / Codex) rather
  than competing — over an exported file or MCP.*
- **graphify, Understand-Anything** (see below) — knowledge-graph builders. Both call Claude's cloud and run
  as assistant skills/plugins, regenerating a graph per run. *Indexa is local, persistent, whole-disk, and
  a standalone engine.*

## Spotlight: graphify & Understand-Anything (the two repos worth studying)

Both are **large, fast-moving AI-coding-assistant skills/plugins** (each tens of thousands of stars, MIT,
actively released in 2026) that turn a folder into an **interactive knowledge graph** using tree-sitter +
an LLM, with a **web-dashboard graph visualization** and export to wiki/Obsidian/HTML/Neo4j. They run
*inside* Claude Code / Cursor / Codex / Gemini CLI and lead with a "massively fewer tokens" hook.

**What they do that Indexa doesn't (yet):**
- The **knowledge graph + a real graph visualization is the product** — Indexa's "Map" is still a plain
  table. This is Indexa's most visible gap and biggest UX opportunity.
- **Distribution as a one-line AI-assistant skill/plugin** — an enormous adoption lever. Indexa is a
  separate binary you install.
- Community detection (Leiden), "highest-degree concepts / surprising connections," multimodal extraction
  via cloud vision, and strong marketing surface (homepage, Discord, token-savings headline).

**What Indexa does that they don't:**
- Truly **local / offline** (they require the cloud); a **persistent indexed store + hybrid retrieval**
  (they regenerate per run); **whole-disk** ambient scope (they're per-repo); **resource-aware** local-model
  discipline; **dual cloud+local** value; a single Rust binary.

**What Indexa should borrow (now on the roadmap):**
- Make a **signature graph/treemap visualization** the brand visual (the existing per-node
  `context: N%` coverage data already supports it).
- **Make token-savings visible** in `export` and the workspace.
- **Local multimodal** understanding (do what they do with cloud vision, but offline).
- A **code-relationship graph** — they prove the demand; Indexa's edge is doing it in **local SQLite,
  behind MCP**, not Neo4j/cloud.
- Consider an **Indexa MCP/skill distribution** so AI assistants can invoke it as easily as these skills.

## Capability gaps (ranked by impact) — all now on the roadmap

1. **No code-relationship graph** (calls/imports/blast-radius). Foundation exists: tree-sitter already
   extracts symbol definitions; edges are missing.
2. **No multimodal content understanding** — media is metadata-only today. Config stubs exist but are unwired.
3. **Brute-force vector search** — no ANN/HNSW index; a practical ceiling around a few hundred thousand
   chunks. Embedding is also one-text-at-a-time (no batching).
4. **Single-shot `ask`** — no agentic multi-step retrieve→reflect→retrieve for hard cross-cutting questions.
5. **Onboarding & the Map** — weak first-run guidance; the Map view under-delivers on its name.
6. **No GraphRAG-style thematic answers** — would follow the code/knowledge graph.

## 2026 trends to ride

- **MCP is the universal AI integration layer.** Indexa is early here — double down and market it.
- **GraphRAG / structured retrieval** has gone mainstream.
- **Capable local vision models** (via Ollama) finally make **offline** image/video/audio understanding feasible.
- **Agent memory** is becoming its own category — Indexa's persistent, addressable store fits it.

---

*This is a point-in-time competitive snapshot for internal strategy; tool capabilities change. Verify
specifics against each project before quoting.*
