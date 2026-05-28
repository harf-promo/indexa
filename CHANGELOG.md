# Changelog

All notable changes to Indexa will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.5] — 2026-05-29

### Fixed

- **Walk crash ("rayon thread-pool too busy")** — `jwalk::WalkDir` now uses `Parallelism::RayonNewPool(min(cpu_count, 4))` so each walk owns its own rayon pool instead of sharing the process-global one. Concurrent walks no longer deadlock. Added a `Semaphore::new(2)` in the web layer as defence-in-depth; additional walks queue rather than racing.
- **"Connection lost" on page refresh** — the browser's `EventSource.onerror` handler no longer calls `es.close()`, which was killing the browser's built-in auto-reconnect. The new handler uses exponential backoff (250 ms → 4 s) and only marks a job gone after a 404 from `/api/jobs/:id` — eliminating false "connection lost" toasts for finished jobs.
- **Dropped SSE events now visible** — when the broadcast channel lags (slow consumer), a `JobEvent::Warning` is emitted (`"dropped N events — refresh to resync"`) instead of silently discarding events. Broadcast channel capacity bumped 128 → 512 for headroom.

### Added

- **Job persistence across refresh** — active job IDs are written to `localStorage['indexa.activeJobs']` on subscribe and merged with the server's `/api/jobs` list on page load. A page refresh during a long indexing run now re-subscribes to the live stream automatically.
- **60 s finished-job retention** — completed/failed job handles stay in the server's registry for 60 seconds after finishing. A page refresh within that window can replay history and re-attach to the final state without a 404.

### Changed

- **Full UI redesign (shadcn-style)** — the web UI has been completely rebuilt:
  - HSL CSS design tokens (`--bg`, `--surface`, `--border`, `--text`, `--accent`, …) with light and dark themes, toggled via a topbar button and persisted to `localStorage`.
  - Typography: Inter for chrome, JetBrains Mono for code and file paths (loaded via Google Fonts; system fallbacks if offline).
  - New layout: fixed 52 px topbar with logo + tab navigation (Browse / Ask / Map / Settings); collapsible 260 px sidebar for the folder tree; docked bottom-right jobs panel (360 px wide, max-height 50 vh) replaces the cramped inline jobs list.
  - ⌘K command palette — fuzzy-search across folder paths and actions; keyboard-navigable (↑ ↓ ↵ Esc).
  - Animated tab transitions (180 ms fade + translateY), rounded cards with subtle shadows, and WCAG AA focus rings on every focusable element.
- **UI assets extracted** — the ~1 350-line inline HTML/CSS/JS string is replaced by three `include_str!`-embedded files (`index.html`, `app.css`, `app.js`) served at `/`, `/assets/app.css`, and `/assets/app.js`. Binary is still fully self-contained.

## [0.3.4] — 2026-05-28

### Fixed

- **Stuck jobs** — The per-row ⚡ (deep) and ↻ (scan) buttons now correctly finalize their job as `done` when complete. Previously `run_deep_phase` and `run_scan_phase` terminated with only a `Note` event and never mutated `handle.status`, leaving the EventSource open and the job row stuck forever in the UI.
- **"Snapshotting…" frozen text** — when a deep job finds zero files (e.g. all files are already current, or the path is empty), the job row now shows `"No files to process"` and clears correctly once the `done` event arrives. Previously the `.job-file` slot showed `"Snapshotting…"` with no subsequent event to overwrite it.
- **Walk errors swallowed in `api_job_deep`** — the handler previously called `.unwrap_or_default()` on walker failures, silently running a 0-file deep phase. Now uses `walk_for_job` (same as the full pipeline) which emits a proper `failed` event on walk errors.
- **Silent failures surfaced as warnings** — parser errors, embedding failures, and chunk-storage errors inside the deep-index loop no longer swallow silently. Each emits a `JobEvent::Warning` so the job row shows a `⚠ N warnings` badge and the warning list is accessible on hover.
- **Anyhow error chains preserved** — `JobEvent::Failed` and HTTP error responses now use `format!("{e:#}")` (full anyhow chain) instead of `e.to_string()` (top-level message only). Summarize failures stored in `summary_queue.error` are also expanded.

### Added

#### Structured error reporting

- **`JobEvent::Failed` enriched** — the variant now carries optional `stage` (e.g. `"walk"`, `"deep"`, `"summarize"`), `item_path` (file being processed when the failure occurred), `chain` (full anyhow cause chain), and `code` (short stable error code). All new fields are optional and backward-compatible.
- **`JobEvent::Warning` variant** — non-fatal per-file issues are broadcast as warnings rather than discarded or aborting the job.
- **📋 Copy report button** — failed job rows now include a copy-report button that assembles a Markdown error report (version, stage, item, error chain) and appends the last 50 lines from the log file. Rows stay visible until manually dismissed via ×.
- **`GET /api/logs/tail?lines=N`** — returns the last N lines of the most recent `indexa.log` file (default 50, max 500).
- **Rolling log file** — `tracing-appender` writes daily-rolling JSON log files to `<data_dir>/logs/indexa.log.YYYY-MM-DD`. The stderr layer is unchanged (human-readable format, respects `RUST_LOG`).
- **Panic hook** — a custom `panic::set_hook` captures the panic message and a full backtrace via `tracing::error!` before re-raising, ensuring crashes land in the log file.

#### Live AI output view

- **`Generator::generate_stream`** — new method on the `Generator` trait that calls a callback for each token/chunk as it arrives. Default implementation falls back to single-shot (one callback at end). `OllamaLlm` overrides this with a real NDJSON stream (`"stream": true` against `/api/generate`).
- **`JobEvent::LlmFragment` variant** — broadcast-only (not stored in job history to prevent memory bloat) with `item_path`, `model`, `stage`, and `fragment` fields. Emitted during contextual-retrieval blurb generation when `describer.contextual_retrieval = true`.
- **✨ Live AI panel per job row** — each job row has a ✨ toggle button that expands a collapsible panel showing the model's current output streaming in real time. Output is capped at 4 KB (sliding window). The `requestAnimationFrame` batching already used for progress events applies here too.

### Changed

- **Failed job rows pinned until dismissed** — the previous 30-second auto-remove for failed rows is removed. Rows stay until the user clicks ×, giving time to copy the error report.
- Broadcast channel capacity bumped from 64 to 128 to accommodate `LlmFragment` bursts.

## [0.3.3] — 2026-05-28

### Added

#### Progress UX — "snapshot then process" model
- **Granular per-file progress events** — `JobEvent::Progress` now carries `current_path`, `items_per_sec`, and `eta_secs` (all optional, backward-compatible). The deep and summarize phases emit one event per file instead of every 10th.
- **Snapshot event** — a new `JobEvent::Snapshot { count, bytes }` fires once immediately after the file list is enumerated, before any processing begins. The UI uses it to switch the progress bar from indeterminate ("Snapshotting…") to a live `current/total` bar.
- **Progress bar per job row** — each live-job card in the sidebar now shows a `<progress>` bar, the current file path, throughput in files/s, and an ETA. The bar is animated/indeterminate during the walk phase and becomes determinate once the Snapshot event arrives.
- **LLM timing in summarize phase** — each summary item emits the per-call LLM duration as a note (`"4.2s · gemma3:4b"`) so you can see how fast the local model is moving.
- **`GET /api/jobs/:id`** — new JSON snapshot endpoint (no SSE needed) that returns `{job_id, kind, path, started_at, status, last_event}`.

#### Per-folder file/chunk counts in the tree
- **Folder rows now show `(N files · M chunks)`** directly beside the folder name. The counts are returned by `GET /api/tree` and `GET /api/search` via SQL subselects on the `entries` and `chunks` tables. Counts are omitted when both are zero (e.g. before a deep-index run).

#### Science-backed retrieval improvements
- **Default embedding model bumped to `nomic-embed-text-v1.5`** (Matryoshka-trained, higher MTEB rank, 8192-token context vs 2048 for v1; same 768 dimensions — existing indexes keep working without re-embedding, but `indexa deep --force <path>` is recommended for the quality boost).
- **Contextual Retrieval opt-in** — a new `describer.contextual_retrieval = true` config flag enables per-chunk context blurbs at index time (Anthropic 2024; 49% fewer retrieval failures measured). When enabled, `gemma3:4b` generates a 1-2 sentence situating blurb for each chunk before embedding. The original chunk text is stored unchanged; only the embedding uses the enriched text. Defaults `false` to avoid re-embedding existing indexes.
- **Summary-boost reranking wired** — `retrieval.summary_weight` and `retrieval.summary_depth_alpha` (declared but never consumed) are now fed into the retrieval pipeline. After hybrid RRF fusion, parent-directory summary cosine similarity boosts chunk scores via `score += summary_weight × sim`. Default `summary_weight = 0.0` (disabled); set to `0.3–0.5` after running `indexa summarize` to try it.
- **`QaConfig` extended** — `summary_weight` and `summary_depth_alpha` are now forwarded from `RetrievalConfig` through both the web API (`POST /api/ask`) and the CLI (`indexa ask`).

### Changed

- **UX: Alt/⌘-click a folder label** in the tree to scope the search box to that folder path (fills the search input with `<path>/` and fires a search).
- **Code simplification (round 3)** — extracted `fireJob(kind, path)` JS helper; three call sites (per-row tree actions, add-root modal, re-index-all) now share it.

### Notes

- Cross-encoder reranking via `fastembed-rs` (plan stage D.2) is deferred to v0.3.4 — the ONNX runtime dependency adds significant CI compile time. The `retrieval.rerank` config flag is already reserved.

## [0.3.2] — 2026-05-28

### Changed

- **Jobs panel moved to top of sidebar** — the live SSE progress panel now sits directly below the tree-pane header (above the search box and tree list) so it's always visible above the fold, even with deep trees. Added `max-height: 35vh; overflow-y: auto` to prevent it from pushing the tree off-screen during a burst of jobs.
- **Sound notifications** — a short Web Audio API tone plays when a job finishes (`done` = ascending two-note ping, `failed` = descending tone). No audio files bundled — generated in-browser. A 🔔/🔕 toggle in the header switches sound on/off; preference saved in `localStorage.indexa_sound_muted`. **On by default.**
- **Inline toast notifications** — all eight `alert()` modal dialogs replaced with a `toast(msg, level)` helper. Toasts appear at top-center of the page, auto-dismiss after 4 s, and have a × close button for sticky errors. Levels: `info`, `warn`, `error`.
- **Failed job rows auto-clear** — failed job rows now self-remove after 30 s (same as successful jobs' 5 s), keeping the jobs panel from accumulating stale errors.
- **Code simplification** — ~50 more lines removed:
  - `crates/core/src/store.rs`: extracted `embedding_to_blob`, `blob_to_embedding`, `row_to_summary`, `row_to_tree_node`, `delete_chunks_under_prefix`, `delete_path_artifacts_exact` helpers.
  - `apps/indexa/src/main.rs`: extracted `require_index_db()`, `build_embedder()`, `build_llm()` helpers; collapsed 9 identical early-return blocks.

## [0.3.1] — 2026-05-28

### Added

- **Per-row tree actions** — hovering any folder row in the sidebar reveals four action buttons: ↻ Re-scan, ⚡ Deep index, 📝 Summarize, 🗑 Remove from index. Each wires into the existing SSE job infrastructure (`/api/jobs/{scan,deep,summarize}`) so progress is visible in the live jobs panel without opening a terminal.
- **Version chip** — the header now shows the running version (e.g. `v0.3.1`) fetched from `GET /api/version` on page load.
- **Re-index all button** — a ↻ button in the tree-pane header fires `POST /api/jobs/deep` for every indexed root in sequence, with a confirm prompt.
- **Full-path tooltip** — hovering a tree-row label shows the absolute path via the native `title` attribute.
- **`GET /api/version`** — returns `{ "version": "0.3.1" }`.
- **`DELETE /api/entry?path=`** — removes a path and all its children from the index (wraps `Store::delete_subtree`; returns `{ "removed": N }`). Files on disk are not deleted.

### Changed

- **Code simplification** — 138 lines removed from `crates/web/src/lib.rs` and `crates/query/src/summarize.rs`: error-response boilerplate consolidated into a helper, repeated `register_job` / `walk_for_job` patterns folded, `TreeNode → TreeNodeResponse` mapping extracted to `From` impl, `while let` loops and `let-else` throughout. All HTTP routes, SSE event shapes, and embedded UI unchanged.

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
