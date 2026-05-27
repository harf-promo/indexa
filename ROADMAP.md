# Roadmap

This roadmap is directional — dates are targets, not promises. All milestones have a corresponding [GitHub Milestone](../../milestones).

## v0.1 — Index + Ask (target: ~3 months from project start)

The first publicly usable release. Everything needed to scan a disk and ask questions about it.

- `indexa scan <path>` — walk, parse, embed, store
- `indexa ask "<question>"` — hybrid semantic + full-text search with LLM-synthesized answer
- `indexa watch` / `indexa daemon` — incremental index updates via filesystem events
- `indexa serve` — local web UI (folder tree, file detail, chat, search)
- File parsers: plain text, Markdown, source code (tree-sitter), PDF, images (EXIF), audio/video metadata (ffprobe)
- LLM adapters: Ollama, llama.cpp HTTP, OpenAI, Anthropic
- Cross-platform binaries: macOS (arm64 + x86_64), Linux (x86_64 + arm64), Windows (x86_64)

## v0.2 — Fingerprints (target: ~5 months)

Detect software, frameworks, and project types by file-pattern signatures.

- Community-curated pattern library (e.g. "Next.js project", "Lightroom catalog", "Xcode workspace")
- `indexa fingerprint` command — list detected software on the machine
- Contributor guide for adding new fingerprint definitions

## v0.3 — Insights (target: ~7 months)

Analytical reports over the index.

- Duplicate file cluster detection
- Stale project detection ("last touched > 1 year ago")
- Weekly diff report ("what changed this week")
- Informational anomaly hints (large new binaries, unsigned executables) — advisory only, not antivirus

## v0.4 — Mobile read-only (target: ~10 months)

Browse a desktop index from a phone.

- iOS and Android companion app (read-only)
- Local network sync of the index database

## v0.5 — Plugin SDK (target: ~12 months)

Open the platform to third-party extensions.

- Stable plugin API for custom parsers, LLM adapters, and insight modules
- Plugin registry / discovery

---

Items beyond v0.5 are tracked in [Discussions → Ideas](../../discussions/categories/ideas). Vote on what matters to you.
