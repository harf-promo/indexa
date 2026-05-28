# Changelog

All notable changes to Indexa will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] — 2026-05-28

### Fixed

- **Empty tree pane** — `GET /api/tree?path=` (empty string) always returned zero rows because scanned paths use absolute parent paths. New `Store::root_paths()` query finds the implicit roots (parent dirs of scanned paths that are not themselves entries). `initTree()` now calls `GET /api/roots` first and renders each root as an expandable folder. Empty-state card shown when no roots exist yet.
- **Raw string delimiter mismatch** — closing `"##` should have been `"#`; caused compile error on fresh build.

### Added

#### Web UI — full feature parity with CLI

- **File-name search** — search box above the tree (200 ms debounce) calls `GET /api/search?q=&limit=50`. Live results replace the tree; clearing the box restores root view. Backed by new `Store::search_paths()`.
- **Add-Root modal** — `+` button opens a modal with a path input and a Jupyter-style filesystem browser (`GET /api/fs/ls?path=`). Security-clamped to `$HOME`, rejects `..` traversal (403). Index button shows terminal command for now (SSE job infra coming in v0.3.1).
- **Queue badge** — sidebar header polls `GET /api/queue` every 3 s and shows `N pending · N running · N failed` when the worker has activity.
- **Refinement Passes save** — the two spinner inputs in Settings now load their live values from `GET /api/config` on tab open, and a "Save passes" button writes them via `POST /api/config/passes` (gated by `INDEXA_WEB_ALLOW_KEY_EDIT=1`).
- **Map tab** — new Map tab surfaces `GET /api/map` as a compact Category / Files / Size table.

- **Live SSE job progress** — "Index this folder" in the Add-Root modal now triggers a real background job (`POST /api/jobs/index`) that runs scan → deep → summarize sequentially. A running-jobs panel appears at the bottom of the sidebar and updates live via `GET /api/jobs/:id/events` (Server-Sent Events). After the job completes, the tree auto-refreshes and shows the new root. In-flight jobs survive a browser refresh (reconnected on page load via `GET /api/jobs`).

#### New API endpoints

- `GET /api/roots` — implicit tree roots (parent dirs of scanned paths that are not themselves entries).
- `GET /api/search?q=&limit=` — file-name substring search.
- `GET /api/fs/ls?path=` — list subdirectories of a path (home-clamped, no dotdot).
- `GET /api/queue` — `{pending, in_flight, done, failed}` counts.
- `GET /api/queue/failed` — failed summary-queue items with error messages.
- `POST /api/queue/retry?path=` — reset a failed queue row to pending.
- `GET /api/config` — safe config subset (passes, cap, max_children).
- `POST /api/config/passes` — write passes config (gated by env var).
- `POST /api/jobs/index?path=` — start scan→deep→summarize job; returns `{job_id}`.
- `POST /api/jobs/scan?path=` / `deep?path=` / `summarize?path=` — individual-phase jobs.
- `GET /api/jobs` — list active jobs.
- `GET /api/jobs/:id/events` — SSE stream of `JobEvent` messages (replays history for late subscribers).
- `DELETE /api/jobs/:id` — cancel and remove a job.

## [0.1.0-rc1] — 2026-05-28

First release candidate. All core functionality is in place and end-to-end tested
locally. Feedback welcome via [Discussions](../../discussions).

### Added

#### New file format support
- **EPUB 2/3 parser** — reads spine order from OPF, extracts XHTML per chapter, decodes HTML entities. Closes #6.
- **Org-mode parser** — heading-aware, handles `#+BEGIN_SRC` code blocks with language tags, strips inline markup. Closes #7.
- **PDF heading-aware chunking** — detects section headings in text-layer PDFs and produces per-section chunks instead of flat word windows. Closes #8.

#### New embedding provider
- **Google Gemini embeddings** — `text-embedding-004` (768 dim, Apache-2.0). Configure with `embedding.provider = "google"` and `GOOGLE_API_KEY`. Closes #9.

#### New CLI commands
- `indexa status` — shows index size, entry/chunk counts, embedding config, last indexed time. Closes #12.
- `indexa rm [--recursive] <paths>` — removes paths from the index. Closes #13.

#### New CLI flags
- `indexa deep --dry-run` — estimates what would be indexed without writing to the DB. Closes #14.
- `indexa ask --scope <path>` — limit search results to a directory subtree. Closes #16.
- `indexa ask --sparse-only` / `--dense-only` — choose retrieval mode per-query. Closes #17.
- `indexa ask --top-k <n>` — override top-k per-query.
- `indexa watch --embed-model`, `indexa serve --embed-model --llm-model` — model flags now consistent across all commands. Closes #22.
- `--help` examples on all subcommands. Closes #25.

#### Environment variables
- `OLLAMA_HOST` — override Ollama server URL without editing config. Closes #10.
- `OPENAI_BASE_URL` — override OpenAI base URL (proxies, LM Studio, etc.). Closes #11.
- `GOOGLE_BASE_URL` — override Google API base URL.
- URL resolution: config `base_url` → env var → compiled-in default.

#### Web UI
- ⌘K / Ctrl+K keyboard shortcut focuses the search input. Closes #20.

#### Surface scan
- Linux XDG paths: `~/.cache` (Skip), `~/.config` (StructureOnly), `~/snap`, `~/.var/app` (Skip). Closes #21.
- Virtual filesystems: `/proc`, `/sys`, `/dev`, `/run`, `/tmp` — all Skip. Closes #21.
- Project manifest fingerprints: directories with `Cargo.toml`, `package.json`, or `pyproject.toml`/`setup.py` classified as `rust-project`, `js-project`, or `python-project`.

#### Retrieval
- `HybridMode::Sparse` and `HybridMode::Dense` now actually honored in `hybrid_search`. Closes #17.
- `RetrievalConfig.rrf_k` is now used (was previously shadowed by a hardcoded constant).
- `--scope` path filter uses parameterized SQL.

#### Store
- `Store::delete_entry(path)`, `delete_subtree(prefix)`. Closes #13.
- `Store::embedded_chunk_count()`, `last_indexed_at()`. Closes #12.

#### Docs
- `docs/architecture.md` — new: crate map, data flow diagrams, storage schema, adapter table. Closes #23.
- `docs/config.md` — Google provider, env var docs, updated defaults.
- `docs/quickstart.md` — `gemma2:9b` pull step, env-var section.
- `CONTRIBUTING.md` — PATH note for `~/.cargo/bin`. Closes #24.

### Changed

- **Default LLM**: `qwen2.5:14b` → `gemma2:9b` (Google, Apache-2.0). Closes #15.
- DOCX/ODT text now decodes XML entities (`&amp;` → `&`, etc.). Previously leaked raw. Closes #18.
- `dirs_home()` fixed — was returning `""`, causing `~`-prefixed surface hints to silently never match. Closes #19.

### Initial scaffolding (from pre-release)
- Initial project scaffolding: Cargo workspace, crate stubs, CI, community files.

### Known limitations

- Vector search is brute-force cosine scan — adequate for <300K chunks; no HNSW yet.
- Single-file SQLite — no concurrent write access.
- Scanned / image-only PDFs produce empty chunks (OCR is a future opt-in).
- `HybridMode::Weighted` declared but not yet implemented (returns an error; use `rrf`).

---

[0.1.0-rc1]: https://github.com/harf-promo/indexa/releases/tag/v0.1.0-rc1
[Unreleased]: https://github.com/harf-promo/indexa/compare/v0.1.0-rc1...HEAD
