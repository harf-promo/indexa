# Competitive landscape

Where Indexa sits, who's nearby, and what makes it defensible. Honest, with the gaps named.

> **Snapshot updated 2026-08-30 (v0.77.0).** Competitor capabilities drift; for what Indexa has
> shipped since this date, [CHANGELOG.md](../CHANGELOG.md) is canonical, not this file.

## The one-line position

Indexa is **the local context engine for AI** — it indexes your disk or repo once, builds a persistent
hierarchical context graph, and serves any AI tool (cloud or local) a small relevant slice on demand,
over CLI, a web UI, and MCP.

## The uncontested intersection

No competitor occupies all seven of these at once. Indexa does:

1. **Local-first** — offline, private, free; your data never leaves the machine unless you point it at a cloud model.
2. **Whole-disk *and* code** — documents, code, images, audio, video; not repo-only, not docs-only.
3. **Persistent index + retrieval** — a queryable store with hybrid search, not a one-shot context dump.
4. **Four interfaces** — CLI, local web workspace, a signed/notarized macOS desktop app, and a native **MCP** server for agents.
5. **Resource-aware** — a memory watchdog that won't freeze the machine running local models.
6. **Dual-audience** — saves *cloud* tools their paid tokens **and** gives *local* models context they can't hold.
7. **Queryable at every level** — a bottom-up roll-up gives *every folder* its own composed summary at L0/L1/L2 — not just per-file chunks, not one whole-repo brick. Addressable context at each tier of the tree.

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
  no persistent index, no retrieval, no relevance slice, no whole-disk. The model is fundamental: a real
  repo packs to **tens of millions of tokens** (one benchmark: ~56M; even filtered + signature-compressed
  ≈1.8M — still past any window), and Repomix's own most-requested issues are "output only related files"
  and "entity-level packing to cut tokens." *That's the wedge: **retrieve the slice, don't pack the repo.**
  Indexa serves the ~2–4K tokens that answer the question from a persistent index. And when you do want a
  file, Indexa's `export --signatures` (code-skeleton) + `--token-budget` + on-export secret-scan make the
  packed slice smaller and safer than a raw dump. And a packer emits a flat brick or per-file list with no
  intermediate structure; Indexa's bottom-up roll-up means "what is the `auth/` folder for?" has a
  precomputed answer and "what is this whole project?" is one synthesis — neither re-reads a file.*
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
- **Distribution as a one-line AI-assistant skill/plugin with zero separate install** — Indexa closed
  most of the practical gap (`mcp install --client …`, `install-hooks`, a shipped SKILL.md — see
  "borrowed" below), but is still fundamentally a binary you install, not a pure assistant-side plugin.
- A strong dedicated marketing surface (homepage, Discord, a "massively fewer tokens" headline) — a
  marketing gap, not a product one.

**What Indexa does that they don't:**
- Truly **local / offline** (they require the cloud); a **persistent indexed store + hybrid retrieval**
  (they regenerate per run); **whole-disk** ambient scope (they're per-repo); **resource-aware** local-model
  discipline; **dual cloud+local** value; a single Rust binary.

**What Indexa borrowed (✅) and what's still open:**
- ✅ **Local multimodal** understanding — what they do with cloud vision, Indexa does **offline**
  (opt-in image captioning + audio transcription).
- ✅ A **code-relationship graph** — they prove the demand; Indexa does it in **local SQLite, behind
  MCP**, now including cross-file call edges (`who_calls` / `blast_radius`, scoped resolution since
  v0.25) and open-ended traversal (`dependency_closure`, `trace_path`, `symbol_context`) — not
  Neo4j/cloud.
- ✅ A **signature graph visualization** — the Map tab's force-directed call graph, treemap, community
  and architecture-module overlays (see "The Map, as a real map" below). The Map is no longer a plain
  table — this was the biggest closed gap on this page.
- ✅ **Token-savings visible** in `export` and the workspace — per-answer/per-session/weekly Impact
  telemetry, plus a per-file "show the math" breakdown.
- ✅ **Indexa MCP/skill distribution** — `indexa mcp install --client claude-code|claude-desktop|
  cursor|vscode` auto-detects and configures installed clients; `indexa install-hooks` generates
  Claude Code hook config and a shipped SKILL.md teaches an agent which tool fits which question —
  this closed what was the one remaining open gap on this page.

## Capability arc — what we closed, and what's next

**Closed since this analysis began (all shipped):**

- ✅ **Code-relationship graph (D1)** — imports + defined symbols across Rust/Python/JS/TS/Go/Java/C/C++,
  queryable over MCP (`dependencies`, `who_imports`).
- ✅ **Local multimodal** — opt-in on-device image captioning and audio transcription; media is no
  longer metadata-only.
- ✅ **ANN/HNSW + batch embedding** — an HNSW index lifts the brute-force ceiling on large corpora
  (now on by default in `serve`/`mcp` above `ann_min_chunks`), and deep-phase embedding now batches.
- ✅ **First-run onboarding + streaming `ask`** — guided empty-state flow; answers stream token-by-token.
- ✅ **Cross-file call edges / blast-radius (D2)** — `who_calls` / `blast_radius` (v0.12; bare-name
  matched, honestly labeled), a strict precision mode (v0.20), and scoped resolution tiers
  (same-file/import/same-dir before falling back to labeled bare-name matching, v0.25) — the
  bare-name asterisk now applies only to the labeled remainder.
- ✅ **The Map, as a real map** — coverage treemap (v0.13), the force-directed call-graph view (v0.18)
  with PageRank centrality sizing (v0.20), a Louvain communities overlay (v0.72, opt-in), and
  persisted architecture-map modules (v0.77).
- ✅ **Context Packs** — subject-scoped portable bundles (v0.14), with `--auto` semantic gathering.
- ✅ **Agentic, multi-step `ask`** — bounded plan → search → refine loop, opt-in, fails open (v0.20).

**Closed since this list was last written (keep them off the open list):**

- ✅ **Token-savings telemetry** — per-answer, per-session, and weekly Impact (≈4 bytes/token, labeled).
- ✅ **Decision Ledger** — CLI / web / MCP, with revision chains and a Review inbox.
- ✅ **GraphRAG-style thematic answers (v0.70)** — opt-in topic-clustered synthesis
  (`[retrieval] graphrag_clusters`) groups a broad question's hits into themed sections before
  synthesis. Ships default-off: the pre-registered A/B on Indexa's own (topically cohesive) corpus
  showed no lift over flat packing, so it's unpromoted — a real lever for corpora that span more
  genuinely distinct topics.

**Still open:** nothing currently tracked here — the two items previously listed (scoped call
resolution, folded into the D2 bullet above; GraphRAG synthesis, just above) have both shipped.
New gaps get logged here as identified.

## What we deliberately won't build

Positioning, not backlog. These are rejected because they dilute the moat, not because they're hard:

- **Team / multi-user features.** A personal whole-disk index is the *last* thing that should ever be
  multi-user — auth, ACLs, and shared corpora are a different product with different buyers, and they
  contradict the privacy story outright. The team-shaped need ("share context with a colleague") is
  already met by `pack export`: sharing by **deliberate act**, as a reviewable self-contained file,
  not by standing access.
- **Cross-machine index sync.** The index is *derived data* — the correct sync is re-indexing on the
  other machine, and Context Packs cover the portable-context case. Real sync would mean conflict
  resolution, cross-version schema compatibility, and a credibility tax on "nothing leaves your
  machine," even peer-to-peer.
- **A VS Code / JetBrains extension.** MCP already puts Indexa inside Cursor, VS Code, Claude Code,
  and every MCP client; an extension would duplicate that surface and add a second release train. The
  real gap is setup friction — solved by docs and a one-shot `mcp install` configurator at ~5% of the
  cost. Revisit only if a feature genuinely needs editor UI.

## 2026 trends to ride

- **MCP is the universal AI integration layer.** Indexa is early here — double down and market it.
- **GraphRAG / structured retrieval** has gone mainstream.
- **Capable local vision models** (via Ollama) finally make **offline** image/video/audio understanding feasible.
- **Agent memory** is becoming its own category — Indexa's persistent, addressable store fits it.

---

*This is a point-in-time competitive snapshot for internal strategy; tool capabilities change. Verify
specifics against each project before quoting.*
