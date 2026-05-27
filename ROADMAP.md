# Roadmap

Milestones ship when they're ready — no dates. Order is directional. Each milestone has a corresponding [GitHub Milestone](../../milestones) with `good first issue` labels seeded before work begins.

Vote on upcoming features and suggest new ones in [Discussions → Ideas](../../discussions/categories/ideas).

---

## v0.1 — Index + Ask

The first publicly usable release. Scan any folder or your whole disk, ask questions in plain language, watch changes in real time.

- **Two-phase scan**: surface scan (fast, zero AI calls, builds a disk map) → deep scan (parses content, generates descriptions, computes embeddings)
- **Flexible scope**: `indexa scan <path>` for a folder, `indexa scan --all` for the whole computer
- `indexa ask "<question>"` — hybrid semantic + full-text search with LLM-synthesized answer
- `indexa watch` / `indexa daemon` — background daemon keeps the index current via filesystem events
- `indexa serve` — local web UI: folder map, file detail view, chat, search
- `indexa map` — CLI summary of what Indexa found and how regions were classified
- File parsers: plain text, Markdown, source code (tree-sitter), PDF, images (EXIF), audio/video metadata (ffprobe)
- AI adapters: Ollama, llama.cpp HTTP, OpenAI, Anthropic
- Cross-platform binaries: macOS (arm64 + x86\_64), Linux (x86\_64 + arm64), Windows (x86\_64)

---

## v0.2 — Fingerprints

Detect installed software and project types by file-pattern signatures — without reading file content.

- Community-curated pattern library: Rails app, Next.js project, Xcode workspace, Lightroom catalog, Premiere project, Final Cut library, Docker Compose stack, and more
- `indexa fingerprint` — list detected software and project types on the machine
- Contributor guide for adding fingerprint definitions (JSON pattern format)

---

## v0.3 — Smart classification

Indexa suggests how to categorize regions of your disk. You confirm, correct, or ignore.

- Automatic "work / personal / archive / media / code / system" tagging at the folder level, inferred from file contents and well-known path patterns
- Suggestions surfaced in the web UI and via `indexa classify`
- Saved classifications feed into v0.4 importance weighting

---

## v0.4 — Importance weighting

Tell Indexa which parts of your disk matter most. It adjusts everything accordingly.

- User-controlled weights per file, folder, or category ("this project is active", "ignore this old archive")
- Weights affect search result ranking and Q&A answer quality
- Auto-suggested weights based on file access recency and frequency (opt-in)
- Exportable weight profiles — share a "new job setup" or "creative work" profile with others

---

## v0.5 — Insights

Analytical reports over your indexed disk.

- Duplicate file cluster detection (exact and near-duplicate)
- Stale project detection ("last touched more than a year ago")
- Weekly diff report — "what changed on your disk this week"
- Informational anomaly hints: large new binaries, unsigned executables, unusual permission changes — advisory only, never antivirus

---

## v0.6 — Mobile read-only

Browse your desktop index from a phone.

- iOS and Android companion apps (read-only)
- Local-network sync of the index database — no cloud required
- Search and ask questions from your phone against your desktop index

---

## v0.7 — Plugin SDK

Open the platform to third-party extensions.

- Stable plugin API for custom parsers, AI adapters, and insight modules
- Plugin manifest format and discovery
- First-party plugin: browser history indexer (opt-in)

---

Beyond v0.7, ideas live in [Discussions](../../discussions/categories/ideas).
