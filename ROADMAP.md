# Roadmap

Milestones ship when they're ready — no dates. Order is directional. Each milestone has a corresponding [GitHub Milestone](../../milestones) with `good first issue` labels seeded before work begins.

Vote on upcoming features and suggest new ones in [Discussions → Ideas](../../discussions/categories/ideas).

> **Note on version numbers:** the milestone numbers below are **feature-theme labels**, not release
> versions — actual releases track their own line and can run ahead. v0.3–v0.5 shipped as platform
> releases (web UI, resource engine, MCP + reranking) rather than the themes once sketched here.
> **Fingerprints (v0.6) shipped in the `v0.6.0` release.** The `v0.7.0` release then shipped the
> instrument-first foundation of the web-UI redesign — an always-on Engine status bar plus a live
> telemetry API — a platform release ahead of the **Smart classification** theme below, which is the
> next feature milestone. The theme numbers (through Context Packs at v0.9) are directional and stay put.

> **Two headline differentiators** (added 2026-05; priority is high, exact slotting flexible — see the
> dedicated sections near the end): **local multimodal understanding** (make images / audio / video
> *content* searchable, fully offline) and a **code-relationship graph** (calls / imports / blast-radius,
> in local SQLite behind MCP). Together they widen the moat against cloud-only, repo-only, and
> assistant-plugin tools. See [docs/COMPETITIVE.md](docs/COMPETITIVE.md).

---

## v0.1 — Index + Ask  *(shipped)*

The first publicly usable release. Build a context map of any folder or your whole disk; ask grounded questions in plain language; keep context current via filesystem events.

- **Two-phase scan**: surface scan (fast, zero AI calls, builds a context map) → deep scan (parses content, generates descriptions, computes embeddings)
- **Flexible scope**: `indexa scan <path>` for a folder, `indexa scan --all` for the whole computer
- `indexa ask "<question>"` — hybrid semantic + full-text search with LLM-synthesized answer
- `indexa watch` — background watcher keeps the index current via filesystem events
- `indexa serve` — local web UI: folder map, file detail view, chat, search
- `indexa map` — CLI summary of what Indexa found and how regions were classified
- File parsers: plain text, Markdown, source code (tree-sitter), PDF, images (EXIF), audio/video metadata (ffprobe)
- AI adapters: Ollama, llama.cpp HTTP, OpenAI, Anthropic
- Cross-platform binaries: macOS (arm64 + x86\_64), Linux (x86\_64 + arm64), Windows (x86\_64)

---

## v0.2 — Hierarchical summarization *(shipped)*

Every file and folder gets a summary. Bottom-up roll-up gives the entire disk a hierarchical context graph that fits in 3.5 GB instead of 600 GB.

- `indexa summarize <path>` — generate summaries for a subtree
- `indexa describe <path>` — print a node's summary + ancestor chain
- `indexa worker` — background queue daemon for low-priority summarization
- `indexa export --format xml|md|json` — export the summary tree as AI-ready context (XML primary, per Anthropic's prompt-engineering docs)
- Web UI: two-pane folder-tree + summary view; Settings tab for local model management and API keys
- Three storage modes: `augment` (keep chunks + summaries), `compress` (drop chunks), `summaries-only` (~3.5 GB / 1 TB)
- Models: `gemma3:4b` for files, `gemma3:12b` for directories — all offline via Ollama (Google/Apache-2.0)

---

## v0.3 — Web UI redesign *(shipped)*

A full web workspace, not just a viewer.

- Live SSE jobs panel, search, add-root, and folder map
- Per-row tree actions, version display, in-app confirm modals, toast + sound notifications
- `indexa doctor` — detected specs, per-model memory table, ETA estimates, Ollama env-var checks

---

## v0.4 — Resource-aware indexing + tiered summaries *(shipped)*

A local-AI index that no longer freezes the machine.

- **Resource engine** — detects RAM / P-E cores / unified-memory limits; a memory watchdog pauses LLM/embedding work under swap pressure and resumes automatically. Three profiles (Conservative / Balanced / Performance).
- **Ollama discipline** — `keep_alive` + single-model residency so models unload promptly and KV-caches don't multiply (the core of the freeze fix).
- **Dedicated Jobs workspace** with live "what the AI is doing now" streaming, filter pills, and warnings.
- **Tiered summaries (L0/L1/L2)** — every node carries a one-line abstract (L0) for agent-facing progressive disclosure.
- Build-artifact pruning, debounced watcher, calibrated ETA.

---

## v0.5 — Agent-addressable *(shipped)*

The local context engine is now reachable by AI agents and ranks its own retrieval.

- **MCP server** — `indexa mcp` runs a stdio [Model Context Protocol](https://modelcontextprotocol.io) server (pure-Rust `rmcp`). Six tools at launch — now **eight** (`dependencies` / `who_imports` joined with the code graph): `search`, `browse_tree`, `get_summary` (L0/L1/L2 tiers), `read_file`, `ask`, `dependencies`, `who_imports`, `get_stats`.
- **Cross-encoder reranking** — optional `[retrieval] rerank` listwise reorder pass; off by default, fails open.
- **Unified Send-safe `query::answer()`** — CLI, web, and MCP all share one retrieval pipeline.

---

## v0.6 — Fingerprints  *(shipped)*

Detect installed software and project types by file-pattern signatures — without reading file content.

- Community-curated pattern library: Rails app, Next.js project, Xcode workspace, Lightroom catalog, Premiere project, Final Cut library, Docker Compose stack, and more
- `indexa fingerprint` — list detected software and project types on the machine
- Contributor guide for adding fingerprint definitions (JSON pattern format)

---

## v0.7 — Smart classification  *(shipped)*

Indexa suggests how to categorize regions of your disk. You confirm, correct, or ignore.

- Automatic "work / personal / archive / media / code / system" tagging at the folder level, inferred from file-type patterns and well-known path names (content-free — no AI calls needed)
- Suggestions surfaced in the web UI (Smart label chip) and via `indexa classify`
- Confirm / Ignore / Undo in the web UI; saved classifications persist user decisions
- Saved classifications feed into importance weighting (v0.8)

---

## v0.8 — Importance weighting  *(shipped — v0.16.0)*

Tell Indexa which parts of your disk matter most. It adjusts everything accordingly.

- User-controlled weights per file, folder, or category ("this project is active", "ignore this old archive")
- Weights affect search result ranking and Q&A answer quality
- Auto-suggested weights based on file access recency and frequency (opt-in)
- Exportable weight profiles — share a "new job setup" or "creative work" profile with others

---

## v0.9 — Context Packs  *(shipped — v0.14.0)*

Your context, sliced by subject — not by folder. Indexa detects that files and folders scattered across your disk all belong to one topic, bundles them into a named **Context Pack**, and lets you export it as a single portable file for any AI tool or teammate.

- **Cross-directory clustering** — semantic grouping finds everything about "Auth", "Tax 2025", or "Client X" no matter where it lives (`~/Projects/…`, `~/Documents/…`, `~/Notes/…`)
- **Suggest, don't impose** — Indexa proposes packs; you confirm, rename, merge, or correct
- `indexa pack create "Auth" --auto` · `indexa pack list` · `indexa pack export "Auth" --format xml`
- **Portable export** — one self-contained context file (XML primary, Markdown alternate — the formats Anthropic's docs recommend for LLM context windows) you can hand to Claude, Cursor, or a colleague
- Builds on Smart classification (v0.7) and Importance weighting (v0.8); reuses the `indexa export` renderers

---

## v0.10 — Insights  *(shipped — v0.16.0)*

Analytical reports over your context store.

- Duplicate file cluster detection (exact and near-duplicate)
- Stale project detection ("last touched more than a year ago")
- Weekly diff report — "what changed on your disk this week"
- Informational anomaly hints: large new binaries, unsigned executables, unusual permission changes — advisory only, never antivirus

---

## v0.11 — Desktop app (Tauri)  *(shipped — v0.12.0)*

A native macOS desktop app — Indexa runs as a menu-bar app instead of a terminal window.

- **Menu-bar tray** — Show/Hide window, Check for Updates, Quit — all from the macOS menu bar
- **Bundles the web workspace** — no separate `indexa serve` needed; the full web UI is served by the embedded Axum server
- **Silent auto-update** — Tauri updater downloads and installs new releases in the background; asks before restarting
- **Window hides on close** — clicking ✕ hides to the tray instead of quitting (standard macOS menu-bar behavior)
- **Native error dialogs** — port-conflict and update-confirmation alerts shown via native macOS dialogs
- macOS Apple Silicon (aarch64); Intel Mac via CLI binary

---

## v0.12 — Mobile read-only

Query your desktop context store from your phone.

- iOS and Android companion apps (read-only)
- Local-network sync of the context store — no cloud required
- Query your desktop context and browse the summary tree from your phone

---

## v0.13 — Plugin SDK

Open the platform to third-party extensions.

- Stable plugin API for custom parsers, AI adapters, and insight modules
- Plugin manifest format and discovery
- First-party plugin: browser history indexer (opt-in)

---

## Local multimodal understanding  *(headline differentiator)*

Today Indexa stores **metadata** for images, audio, and video (EXIF, ffprobe). This milestone makes their
**content** searchable — fully offline, via local vision/audio models. Competing graph/RAG tools that
"understand" media call a cloud API; Indexa does it on your machine.

- **Images** — caption with a local vision model (Ollama); the caption becomes a searchable chunk
- **Audio** — local transcription → searchable chunks
- **Video** — sample frames → caption; optional transcript
- Opt-in per region; goes through the same parse → embed → store pipeline and the resource watchdog
- Default vision/audio models follow the project's model policy (non-Chinese defaults; user-configurable)

## Code-relationship graph  *(shipped — v0.12.0)*

A real code graph kept in local SQLite (no Neo4j, no cloud), queryable over MCP:

- **Phase 1** — imports / defines edges (file-level): `dependencies`, `who_imports` MCP tools *(shipped)*
- **Phase 2** — call edges with cross-file resolution: `who_calls`, `blast_radius` MCP tools *(shipped)*
- **Phase 3 — signature graph visualization** *(shipped — v0.18.0)*: interactive force-directed
  view of the file-to-file call graph in the web Map tab ("Graph" sub-view), plus `store.code_graph`,
  the `/api/graph` endpoint, the `code_graph` MCP tool, and the `indexa graph` CLI command.
- **Weighted PageRank centrality** *(shipped — v0.20.0)*: each file in the call graph carries a
  centrality score; the Map "Graph" view sizes nodes by it, and `indexa graph` / the `code_graph` MCP
  tool surface the most-central hub files. Deeper reachability analysis remains a future iteration.

---

Beyond v0.13, ideas live in [Discussions](../../discussions/categories/ideas).
