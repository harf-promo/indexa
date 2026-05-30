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

- **MCP server** — `indexa mcp` runs a stdio [Model Context Protocol](https://modelcontextprotocol.io) server (pure-Rust `rmcp`). Six tools: `search`, `browse_tree`, `get_summary` (L0/L1/L2 tiers), `read_file`, `ask`, `get_stats`.
- **Cross-encoder reranking** — optional `[retrieval] rerank` listwise reorder pass; off by default, fails open.
- **Unified Send-safe `query::answer()`** — CLI, web, and MCP all share one retrieval pipeline.

---

## v0.6 — Fingerprints  *(shipped)*

Detect installed software and project types by file-pattern signatures — without reading file content.

- Community-curated pattern library: Rails app, Next.js project, Xcode workspace, Lightroom catalog, Premiere project, Final Cut library, Docker Compose stack, and more
- `indexa fingerprint` — list detected software and project types on the machine
- Contributor guide for adding fingerprint definitions (JSON pattern format)

---

## v0.7 — Smart classification  *(next)*

Indexa suggests how to categorize regions of your disk. You confirm, correct, or ignore.

- Automatic "work / personal / archive / media / code / system" tagging at the folder level, inferred from file contents and well-known path patterns
- Suggestions surfaced in the web UI and via `indexa classify`
- Saved classifications feed into importance weighting

---

## v0.8 — Importance weighting

Tell Indexa which parts of your disk matter most. It adjusts everything accordingly.

- User-controlled weights per file, folder, or category ("this project is active", "ignore this old archive")
- Weights affect search result ranking and Q&A answer quality
- Auto-suggested weights based on file access recency and frequency (opt-in)
- Exportable weight profiles — share a "new job setup" or "creative work" profile with others

---

## v0.9 — Context Packs

Your context, sliced by subject — not by folder. Indexa detects that files and folders scattered across your disk all belong to one topic, bundles them into a named **Context Pack**, and lets you export it as a single portable file for any AI tool or teammate.

- **Cross-directory clustering** — semantic grouping finds everything about "Auth", "Tax 2025", or "Client X" no matter where it lives (`~/Projects/…`, `~/Documents/…`, `~/Notes/…`)
- **Suggest, don't impose** — Indexa proposes packs; you confirm, rename, merge, or correct
- `indexa pack create "Auth" --auto` · `indexa pack list` · `indexa pack export "Auth" --format xml`
- **Portable export** — one self-contained context file (XML primary, Markdown alternate — the formats Anthropic's docs recommend for LLM context windows) you can hand to Claude, Cursor, or a colleague
- Builds on Smart classification (v0.7) and Importance weighting (v0.8); reuses the `indexa export` renderers

---

## v0.10 — Insights

Analytical reports over your context store.

- Duplicate file cluster detection (exact and near-duplicate)
- Stale project detection ("last touched more than a year ago")
- Weekly diff report — "what changed on your disk this week"
- Informational anomaly hints: large new binaries, unsigned executables, unusual permission changes — advisory only, never antivirus

---

## v0.11 — Desktop app (Tauri)

A native, installable desktop app — so Indexa runs as a proper background service, not a terminal window you have to leave open.

- **Menu-bar control** — start/pause indexing, switch resource profile, and see live status from the menu bar; one click to ease off when your machine is busy
- **Real background daemon** — replaces leaving `indexa serve` / `indexa worker` running in a terminal; launches at login (opt-in)
- **Signed & notarized installer** — a proper `.dmg` / `.msi` instead of `curl` + a Gatekeeper-quarantine bypass
- **Native notifications** — "deep context ready", "low on memory — easing off" — outside the browser
- Wraps the existing web UI (already a self-contained SPA served by a thin Axum layer), so the workspace is unchanged. **Note:** this is a packaging / daemon-UX upgrade — memory-pressure handling itself lives in the engine, not the shell.

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

Beyond v0.13, ideas live in [Discussions](../../discussions/categories/ideas).
