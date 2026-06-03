# Changelog

How Indexa got sharper, release by release — every change that affects what you can do with it.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.12.3] — 2026-06-03

### Fixed

- Version bump to verify end-to-end desktop auto-update introduced in v0.12.2.

## [0.12.2] — 2026-06-03

### Added

- **Tauri desktop app auto-update.** The desktop app now silently checks for a newer release on
  every launch and installs it automatically (download → install → restart). A new tray menu
  item "Check for Updates…" also triggers the flow on demand. Uses Tauri's own minisign keypair
  for artifact verification — no Apple Developer ID required. `release.yml` gains a new
  `desktop` job that produces `.dmg`, `.app.tar.gz`, signed `.sig`, and `latest.json` for every
  tagged release (macOS Apple Silicon first; Intel and Linux/Windows desktop in a future release).

## [0.12.1] — 2026-06-03

### Fixed

- Version bump to verify end-to-end self-update (`indexa update`) introduced in v0.12.0.

## [0.12.0] — 2026-06-03

A **visual + self-update** release: a squarified treemap for the Map view, one-click self-update from both the CLI and the web UI, D2 call-graph edges in the MCP, and the Tauri desktop app scaffold.

### Added

- **`indexa update` — self-update from the CLI.** Running `indexa update` checks GitHub Releases for a newer version, confirms with the user, and atomically replaces the running binary. Flags: `--check` (report only, exit 1 = update available); `-y` (skip prompt); `--pin v0.12.1` (install a specific release). Works on macOS/Linux/Windows with no external tools. **Note:** this is the first release to include the updater; the first hop from v0.11.0 is still a manual re-download.

- **Web UI version badge + one-click update.** The topbar now shows the installed version. When a newer release is available the badge turns blue and links to a new "Software Update" section in Settings, where you can apply the update in one click (requires `INDEXA_WEB_ALLOW_UPDATE=1`).

- **Interactive squarified treemap for the Map view.** The Map tab now shows a squarified SVG treemap of your indexed folder structure, sized proportionally by disk usage. A **Treemap | Table** toggle keeps the old category table accessible. Cells are colour-coded by top-level directory, show name + size labels, and support click-to-drill-down navigation with a breadcrumb trail and hover tooltips. No external dependencies — offline-safe, pure vanilla JS. Backed by a new `GET /api/map/treemap` endpoint that builds a depth-3 hierarchy with bottom-up subtree-size aggregation.

- **Tauri desktop app scaffold (`apps/indexa-desktop`).** A native macOS/Windows/Linux desktop wrapper that embeds the full Indexa web server directly (no subprocess), opens a WebviewWindow at `http://localhost:7620`, and adds a menu-bar tray icon with Show / Quit actions. Excluded from the Cargo workspace (`[workspace] exclude`) so CI is unaffected; build with `cargo build --manifest-path apps/indexa-desktop/Cargo.toml`. A published signed installer requires Apple Developer ID / Windows code-signing cert and a full CI matrix update — see `apps/indexa-desktop/README.md`.

- **D2 code-graph call edges + `who_calls` / `blast_radius` MCP tools.** Deep-indexing a source file now also records every function/method name it *calls* as `kind='calls'` edges (Rust, Python, JavaScript/TypeScript, Go, Java). Two new MCP tools query them: `who_calls(symbol)` returns all indexed files that call a given name (up to 100 results); `blast_radius(symbol)` returns the 1-hop transitive set — direct callers plus files that call any symbol defined in those callers — giving a conservative "what breaks if I change this?" answer (up to 200 results). The `dependencies` tool now also lists a file's call edges. Existing databases are migrated automatically (the `edges` table's `CHECK` constraint is widened from `imports/defines` to `imports/defines/calls` on first open). MCP tool count: 8 → 10.

## [0.11.0] — 2026-06-01

A **local multimodal + scale** release: opt-in image captioning, audio transcription, and an ANN index for dense search on large corpora.

### Added

- **Optional ANN (HNSW) index for dense search at scale (opt-in).** Set `[retrieval] ann = true` and, once the index passes `ann_min_chunks` (default 50,000), the web server builds an in-memory HNSW index (`hnsw_rs`, cosine) and uses it for the dense arm of retrieval instead of a brute-force cosine scan — cutting latency on large indexes. Built lazily and rebuilt when the chunks change; scoped queries and smaller indexes transparently fall back to brute-force, so results are unchanged (only faster). Default off — small/normal indexes are unaffected.
- **Audio transcription with a local whisper CLI (opt-in).** Set `[parsers.audio] transcribe = true` and `deep` shells out to a whisper.cpp-style CLI (default `whisper-cli`, configurable `binary`/`model`) for each audio file, storing the transcript as a searchable chunk alongside the file's metadata — so you can find audio by what's said in it, fully offline. The binary + model are user-installed (not bundled); only `audio/*` files are transcribed, and a missing/failing binary warns and skips without aborting the run.
- **Image captioning with a local vision model (opt-in).** Set `[parsers.image] caption = true` and `deep` sends each image to an Ollama vision model (default `llama3.2-vision`) and stores the caption as a searchable chunk alongside the file's EXIF metadata — so you can find images by what's *in* them, fully offline. Nothing leaves the machine. Configure the model via `[parsers.image] model`. Note: the vision model (~7–8 GB) isn't yet counted by the memory watchdog, so enable it with headroom; captions are produced for newly-scanned or modified images.

### Changed

- **`chunks.id` is now `AUTOINCREMENT`** (migrated automatically on first open) so chunk ids are never reused after a re-`deep`. This keeps the ANN index's id→chunk mapping correct (a reused id could otherwise mis-attribute a result) and is a general robustness improvement.

## [0.10.0] — 2026-06-01

### Added

- **Code-relationship graph (D1) + `dependencies` / `who_imports` MCP tools.** Deep-indexing a code file now records its graph edges in a new `edges` table: which modules/paths it **imports** and which symbols (functions, types, classes) it **defines**, across Rust, Python, JavaScript/TypeScript, Go, and Java. Two new MCP tools query it — `dependencies(path)` lists a file's imports + defined symbols, and `who_imports(module)` is the reverse lookup (which indexed files import a module). Edges are extracted on the existing tree-sitter parse (no extra pass), refreshed on re-`deep`, and cleaned up when a file is removed. Cross-file *call* edges (D2) are a planned follow-up.
- **Streaming answers in the web Ask view.** `POST /api/ask/stream` serves the same RAG answer as `/api/ask` over server-sent events — one `sources` event up front (citations render immediately), then the answer token-by-token as the model produces it, then a terminal `done`/`error`. Real streaming on Ollama; cloud/`claude-code` providers send the answer in one piece (graceful fallback). The UI consumes it via `fetch` + a streamed-body reader so the question stays in the POST body, not the URL.
- **First-run onboarding for an empty index.** With no roots, the web UI now shows a guided three-step walkthrough (add a folder → Indexa builds context locally → ask or export) and lands on the Context view instead of an Ask view whose copy assumed context already existed. Derived from live state, so it self-dismisses once a folder is added and never nags a populated index.

### Changed

- **`deep` embeds in batched round-trips — materially faster on multi-chunk files.** The deep phase previously made one embedding HTTP call per chunk; it now sends up to 64 chunks per call via Ollama's `/api/embed` batch endpoint (CLI `deep` and the web Deep job alike), falling back per-chunk on any batch error, count mismatch, or older Ollama without the endpoint — so correctness never depends on the batch path. Order is preserved and the embedding dimension is unchanged. Search results are identical: `/api/embed` returns L2-normalized vectors and the legacy single endpoint raw ones, but the directions match exactly and Indexa ranks by scale-invariant cosine.
- **Accessible Settings/Activity drawers.** Opening a drawer now traps focus inside it (the rest of the page is made `inert`) and restores focus to the opener on close; only one drawer can be open at a time. The workspace view tabs expose `aria-selected`/`aria-controls` and the panels are proper `tabpanel`s.

### Fixed

- **Directory summaries no longer go empty or stale under a multi-worker build.** With `worker --concurrency 2+`, a directory could be rolled up before its children's summaries existed and then marked done with an empty/stale summary that never self-healed. The worker now defers a directory's roll-up (re-enqueueing it) while any descendant is still pending or in-flight, so roll-ups always compose finished children. The atomic claim that prevents double-processing is unchanged.
- **A failed summarization-queue item is terminalized instead of stranded.** An unexpected store error mid-process left the claimed item stuck `in_flight`, blocking the queue until the next restart sweep; such an item is now marked `failed`. Separately, `scan`/`deep`/`watch`/`rm` now agree on a canonical path form (e.g. a symlinked root like macOS `/tmp` → `/private/tmp`), so they operate on the same entries.

## [0.9.0] — 2026-06-01

A **model-intelligence + freshness** release: a hardware-aware Local-vs-Cloud model picker, a summary-quality fix, and live-freshness fixes across `deep` and `watch`.

### Added

- **Model intelligence — fit + ETA for *any* model, plus a curated download catalog.** A parameter-count footprint heuristic estimates any model's memory peak and per-job ETA — not just the handful in the built-in table — and installed models are enriched from Ollama `/api/show` (real parameter size + quantization level). A bundled, curated catalog of recommended local models ships in the binary, with an optional fail-open online refresh. A new unified **`GET /api/models`** returns every model (installed ∪ catalog), each annotated with real/estimated size, whether it fits the live memory budget, and an ETA for your index. Chinese-vendor models are listed but never selected as a default.
- **`claude-code` LLM provider — use your Claude Pro/Max subscription.** Set `[describer] provider = "claude-code"` (with `model`/`file_model`/`dir_model` = e.g. `"sonnet"`) and Indexa runs answer synthesis and file/directory summaries on your Claude **subscription** via the local `claude` CLI in headless mode — no API key, no per-token billing. As long as you're logged into Claude Code on the machine, it just works (`claude setup-token` → `CLAUDE_CODE_OAUTH_TOKEN` is the headless-server fallback). Embeddings always stay local (Ollama). Each call spawns a short-lived `claude` process, so a built-in concurrency cap keeps bulk summarization from forking too many at once; for whole-disk bulk, local Ollama is still faster. The new `describer_from_config` factory routes the CLI `summarize`/`worker` and the web summarize job through the same provider switch.
- **Claude subscription status — surfaced in `doctor` and the web UI.** `indexa doctor` now prints a *Claude subscription provider* block (CLI present? signed in? which plan? is `claude-code` the active provider?), and the web Settings panel gains a **Claude subscription** section showing the same. Backed by a new `GET /api/providers/status` and a token-free `indexa_llm::claude_status` probe (`claude --version` + `claude auth status --json` — no model is invoked, so it's safe to call on every Settings load). The user's email from `auth status` is deliberately not exposed.

### Changed

- **Settings — reorganized into a Local-vs-Cloud model picker.** The web Settings drawer is now split into **Local models (Ollama)** — installed and downloadable models shown as rich rows (size · params · a fit badge against your live RAM budget · ETA · role), one-click **Set file / Set dir** assignment, per-row **Pull** with streaming progress, an Ollama endpoint field, and a catalog refresh — and **Cloud providers** grouped by auth type (Claude subscription · API keys). Switching the embedding model asks for confirmation (it invalidates the existing index until a re-embed). Backed by a new gated `POST /api/config/provider`; `GET /api/config` now reports the active provider and per-role models. The model-fit "ask me first" popover now suggests the most capable *installed* model that fits, rather than a fixed floor.

### Fixed

- **Summaries no longer leak a "Here's a refined summary…" preamble.** Multi-pass refinement could prepend conversational meta-text that polluted both the stored summary (L1) and the one-line abstract derived from it (L0), and defeated the loop's no-change early-stop — so on `--passes 3` the preamble compounded. A prompt-level "output only the summary" constraint (both providers) plus a conservative post-processing backstop keep stored summaries clean, and the early-stop now fires correctly.
- **`indexa deep` re-embeds edited files.** Deep compared a file's chunks against the modification time recorded by the last `scan`, so editing a file and re-running `deep` *without* re-scanning silently skipped it (stale chunks and search results). It now compares against the file's live on-disk mtime — in both the CLI and the web standalone Deep job.
- **`indexa watch` keeps summaries fresh after edits.** Watch re-embedded a changed file's chunks but never re-queued its summary, so the summary (and every ancestor directory roll-up that composes it) silently went stale. Watch now re-queues the file and its ancestor roll-ups for the background worker to refresh (run `indexa worker` or `serve` to drain the queue).
- **`indexa watch` fully removes deleted files.** A file deleted while watching had only its chunks removed — its summary, queue, and entry rows lingered, so search and the browse tree kept returning a file that no longer exists. Watch now removes the entry completely and refreshes the affected ancestor roll-ups.
- **`indexa deep` stops 500-ing the embedder on long files, and indexes extension-less text.** An oversized chunk (a long-line or minified file collapsing into a single chunk) could exceed the embedder's context window and fail; chunking is now character-bounded with a client-side truncation backstop. Extension-less UTF-8 text files (LICENSE, NOTICE, Cargo.lock) are now content-sniffed and indexed instead of skipped.

## [0.8.0] — 2026-05-31

### Added

- **`indexa classify`** — the first step of **Smart classification** (v0.7 milestone). Suggests a semantic category (work / personal / archive / media / code / system) for each indexed folder — a second axis over the technical file-type classification. This Tier 0 pass is **deterministic and content-free**: it derives the code/media/system/archive categories from existing surface hints (a folder's own hint, e.g. `node_modules` → code, or the dominant category among its direct files). Folders whose work-vs-personal nature needs file *content* are left **pending** for a later content-inference pass — never guessed. Inspect with `--paths` and `--category`. Suggestions are saved; confirming/correcting them (web UI + CLI) arrives in a following release.
- **Web "ask me first" model-fit popover.** When you start a **summarize** or **build/index** job in the web UI and the configured model wouldn't fit the live memory budget, Indexa now pauses and lets you choose — *use the model that fits* (e.g. `gemma3:4b`), *build anyway* with the configured model, or *cancel* — instead of silently loading a ~9 GB model that thrashes the machine. Backed by a new `GET /api/jobs/estimate` (reuses the shared `fit_report`); job-start endpoints accept an optional model override so your choice is honored. Jobs that load no heavy model (scan/deep) are unaffected.

### Changed

- **`serve` web UI — vocabulary aligned to the context framing.** User-facing labels now say "context", not "index": "Index this folder" → **Build context**, "Index map" → **Context map**, "Deep index" → **Build deep context**, "Re-index all roots" → **Rebuild context for all roots**, "Remove from index" → **Remove from context**, and the empty/loading states read "No context yet / No context roots yet". `indexa scan` / `indexa deep` command names in help text are unchanged.

- **`serve` web UI — memory-pressure warnings are now self-explanatory.** The watchdog's "easing off" warnings carry a structured pressure snapshot (level, swap %, used bytes, compute budget, headroom), rendered as a compact `throttle/critical · budget ±N MB` chip on the warning row so you can correlate a pause with the live Engine-bar RAM gauge instead of parsing the message text. Delivered as an added optional field on the existing warning event (not a new event type), so older clients are unaffected.

- **`serve` web UI — the Engine status bar now narrates the build.** While a summarize/index/deep job runs, the always-on bottom bar shows a live determinate progress bar with the running count, throughput (files/s), ETA, the current file, and the active model — fused client-side from the job's existing event stream, so the bar tells the build story instead of only machine stats. The state word still reads `Building` (or `Easing off` under memory pressure).

- **`serve` web UI — calmer folder tree.** Each directory now shows a static context-coverage glyph (● built · ◐ partly built · ○ none · ✗ failed) plus one determinate `covered/total` count per actively-building subtree, replacing the old pulsing pending icon that appeared on every row during a build. Each folder's summary header gains a `context: N%` chip. Backed by a `{covered, partial, total}` directory-summary rollup carried on every tree node.

- **`indexa summarize` / `indexa worker` now pre-flight the model fit and pick a lighter model when the configured one won't fit.** A new pure `fit_report` reports whether the configured summarization models fit the live memory budget, and the lighter set to use if not. When `[resource] auto_select_model` is on (the default), the CLI downgrades the directory roll-up model (e.g. `gemma3:12b` → `gemma3:4b`) whenever the heavy one wouldn't fit — loading the lightest model rather than a ~9 GB one that thrashes/freezes a tight machine — and prints a calm notice. (Previously `auto_select_model` was a dead flag, honored nowhere.) Set it to `false` to keep your configured models. The interactive web "ask me first" picker reuses the same `fit_report` and lands separately.

### Fixed

- **Add-root folder browser no longer errors.** Browsing for a folder in the web UI's "Add Root" dialog failed with `(d.entries || []).forEach is not a function` — the client read `d.entries` from a response that is a bare JSON array (so `d.entries` resolved to `Array.prototype.entries`, the built-in method). The browser now consumes the array directly (each entry is a directory; the parent folder is the leading `..` entry).

- **The memory-pressure signal no longer misfires on sticky macOS swap.** The watchdog now reads pressure from the real memory **budget** (`total − active/wired − headroom`, which excludes reclaimable macOS file cache) instead of the swap **fraction**. macOS grows its swap file dynamically and never drains stale pages, so the fraction stays high long after RAM frees — which made the always-on Engine status-bar pressure indicator read amber/red (and drove extra model unloads) even with several GB genuinely free and no job running. Pressure now reads `ok` whenever the budget is positive, escalating to throttle/critical only as truly-free RAM falls into and below the headroom floor. The job-entry pause/warning was already budget-gated; genuine low-memory protection is unchanged.

## [0.7.0] — 2026-05-30

An **instrument-first** release: Indexa now shows you what the engine is doing in real time, idle or busy — the foundation of the web-UI redesign — plus an accessibility pass.

### Added

- **Always-on Engine status bar** — a bottom bar in the web UI shows live **CPU**, **RAM** (with the keep-free headroom band drawn in), and **memory pressure**, visible whether the engine is idle or building (#77). The RAM meter draws the used fill over a hatched keep-free band, and RAM-fit (budget/headroom) and swap-pressure are shown as two honest, separate signals — so the gauge can never silently disagree with the watchdog (both derive from the same `assess()` / `compute_budget()`).
- **Live telemetry API** — `GET /api/telemetry` (one-shot) and `GET /api/telemetry/stream` (SSE) expose per-core CPU, RAM, swap, memory pressure, and the compute budget, published from a low-frequency background sampler that runs even when idle (#77). A dedicated `TelemetrySampler` owns its own long-lived `sysinfo` handle, kept out of the per-file memory watchdog's hot loop.

### Changed

- The per-folder "pending" badge no longer pulses during a summarize/index job (#76). An animated ⏳ on every pending folder at once read as a loading spinner near every row; folder state is still conveyed by colour, with calmer aggregate progress to follow.

### Fixed

- **Accessibility:** a global `prefers-reduced-motion` guard now disables every animation and transition (pulse, fade-in, slide-up, tab fades, running indicators) for users who opt out, closing an a11y gap (#76).

## [0.6.1] — 2026-05-30

A patch fixing a build-artifact indexing bug that could make `summarize` appear to run forever.

### Fixed

- **`target/` build directories are no longer indexed or summarized when Cargo's `CACHEDIR.TAG` marker is absent** (test fixtures, partial builds, copied trees). The skip rule now also recognizes a `target/` sitting next to a `Cargo.toml`. Previously such trees leaked tens of thousands of `.o`/`.bin` build artifacts into the index and the summary queue — making `summarize` appear to run forever, regenerating summaries of build junk.

## [0.6.0] — 2026-05-30

The **Fingerprints** release — detect installed software and project types by file-pattern signatures — plus a web Settings **workload control**, a **memory-pressure fix** so a local-AI index right-sizes its model context and recovers gracefully instead of stalling, and a large internal cleanup (no behavior change). Positioning now leads with the dual cloud/local context angle.

### Added

- **`indexa fingerprint`** — detects software and project types (Rust crates, Node/Next.js apps, Docker Compose stacks, Helm charts, …) across indexed folders by file-pattern signatures, without reading file contents. Built-in JSON pattern library extendable via a user `fingerprints.json`; `--paths` lists matching directories. See [docs/fingerprints.md](docs/fingerprints.md).
- `indexa deep` shows live in-place progress (files done / total + current file) on a terminal, auto-hidden when stderr is redirected (#15). Hand-rolled (no new dependency).
- `indexa map` colorizes its output by category when stdout is a terminal; piped/redirected output stays plain (#14).
- **Settings → Resource Profile** — the web Settings tab now exposes a workload control (Conservative / Balanced / Performance, plus a RAM-headroom override), persisted to `[resource]` in `config.toml`. Dial Indexa's intensity down when your machine is busy (applies to the next job). (#71)
- `[describer] num_ctx` config option (default 4096) — the context window Indexa requests from Ollama.

### Changed

- Cloud embedding/LLM adapters (OpenAI, Google, Anthropic, OpenAI-compat) now retry non-streaming requests on transient failures — retryable HTTP statuses (408/425/429/5xx) and connection/timeout errors — with bounded exponential backoff that honors `Retry-After`. A 429/503 during a bulk index no longer permanently fails that item.
- Surface scan recognizes the Linux XDG base dirs (`~/.local/share`, `~/.local/state`, `~/.local/bin`) (#25), and classifies more file types (web/markup, more languages, tabular/scientific data, logs) so fewer files land in the `unknown` category (#21).
- Documentation: positioning now leads with the dual angle — Indexa saves **cloud** AI tools their token budget *and* gives **local** models the context they can't hold in a small window (new README section + a "why this helps local models" rationale in `docs/methodology.md`). Added the **Context Packs** (v0.9) and **Desktop app / Tauri** (v0.11) roadmap milestones.

### Fixed

- **Memory-pressure handling no longer stalls or over-allocates** (#72). Two root-cause fixes: (1) Indexa now sends `num_ctx` (default 4096) to Ollama, so models load at the budgeted context instead of their 32,768-token default — roughly **8× less KV-cache**, and the resource budget finally matches what's actually loaded. (2) The memory-pressure pause now **resumes as soon as free RAM recovers** (`compute_budget > 0`) instead of waiting on macOS's *sticky* swap level (which never drained, stalling jobs for minutes), and it **unloads the resident model while paused** so RAM can actually free. The watchdog warnings are calmer and point to Settings → Resource Profile.

### Internal

- Large source files split for maintainability with **no behavior change**: `main.rs` → `commands/` (#66), `store.rs` → `store/` submodules (public API byte-identical) (#67), `web/lib.rs` → `dto` / `handlers` / `jobs_exec` (#68), and `app.js` / `app.css` → source fragments concatenated server-side into byte-identical assets (#69).

## [0.5.1] — 2026-05-30

A "correctness & hardening" pass over the shipped v0.5.0 engine (found by a full code review),
plus a docs refresh. No new features; existing behavior is unchanged except where noted as a bug fix.

### Fixed

- **Re-index no longer corrupts the FTS index or leaves stale chunks** — `upsert_chunks` used
  `INSERT OR REPLACE`, which reassigned the chunk rowid and orphaned the old FTS5 row on every
  re-index (unbounded FTS bloat, skewed BM25, stale/dropped hits); a file edited to *fewer* chunks
  also left its old tail chunks behind. It now deletes a path's chunks + FTS rows then re-inserts.
- **Summary-queue items no longer leak as `in_flight`** after a crash/kill/cancel — a startup sweep
  resets stale `in_flight` rows to `pending` (failing those past an attempt cap). Queue claims are
  now a single atomic `UPDATE … RETURNING` (no double-processing across worker + web connections),
  and a `PRAGMA busy_timeout` makes contended writers block-and-retry instead of erroring.
- **`indexa summarize` now reports real failures** — per-item failures were swallowed as success,
  so the "0/N succeeded — did you `ollama pull`?" guidance could never fire.
- **One malformed or oversized file can no longer abort a scan** — parser invocations are wrapped
  in `catch_unwind` (a bad PDF could panic `pdf-extract`), and a configurable `[parsers] max_file_mb`
  (default 100) skips oversized files instead of reading them fully into memory.
- **Cloud adapters now have request timeouts** — OpenAI/Google/Anthropic clients were built without
  any timeout, so a stalled connection hung the worker/web/MCP request forever. Ollama mid-stream
  `error` responses are surfaced instead of returning an empty answer as success.
- **web + MCP now honor the configured retrieval mode and context budget** (they previously forced
  RRF and a hardcoded budget); `[retrieval] context_budget` is configurable. The unimplemented
  `weighted` hybrid mode was removed.
- **DB errors surface as HTTP 500** on `/api/stats`, `/api/map`, and the queue endpoints (previously
  masked as an empty index); `DELETE /api/entry` rejects an empty path; deletes now clear summaries
  and queue rows too; the config file is created at mode `0600` atomically (no TOCTOU window).
- **MCP `read_file` / `get_summary(l2)` are confined to indexed roots** — they previously read any
  client-supplied path (contract hygiene for the local-stdio server).
- Fixed a latent word-window underflow/stall in the Org/PDF/Office/EPUB chunkers (consolidated into
  one shared `chunk_words` helper).

### Added

- `[parsers] max_file_mb` and `[retrieval] context_budget` configuration options.
- A cross-surface integration test for the unified `query::answer()` pipeline, plus regression tests
  for re-index FTS integrity, queue lifecycle, the memory-watchdog pause, parser malformed/oversized
  input, and adapter error handling.

### Documentation

- README no longer says "pre-alpha"; documents the MCP server (`indexa mcp`) and optional cross-encoder reranking; "What's coming" now lists what already shipped (web UI, MCP, reranking, tiered summaries, resource-aware indexing).
- `ROADMAP.md` renumbered so feature milestones (Fingerprints → Plugin SDK) map to v0.6+; the consumed v0.3/0.4/0.5 slots are documented as the platform releases that actually shipped. Removed the nonexistent `indexa daemon` command.
- `docs/quickstart.md` pulls the correct default models (`gemma3:4b` + `gemma3:12b`, not `gemma2:9b`) and the right Rust version.
- `docs/config.md` corrects the macOS config path (`dev.indexa.Indexa`), the describer default (`gemma3:12b`), and documents the `[resource]` section, the `passes_*` summarization fields, `summary_weight`/`summary_depth_alpha`, and the real PDF engine (`pdf-extract`).
- `docs/architecture.md` adds `crates/mcp`, rewrites the `ask` flow around the unified `query::answer()` pipeline (retrieve → optional rerank → synthesize), and fixes the storage paths, walk (jwalk + pruning), and watcher (`notify-debouncer-full`).
- Archived `docs/known-issues-v0.2.2.md` (all resolved in v0.2.3).

## [0.5.0] — 2026-05-30

The "agent-addressable" release: the local context engine is now reachable by AI
agents over MCP and ranks its own retrieval — without adding a single native
dependency or turning the engine into an app.

### Added

- **MCP server** — `indexa mcp` runs a stdio [Model Context Protocol](https://modelcontextprotocol.io) server (official `rmcp` SDK, pure Rust) so Claude Desktop / Cursor / any MCP client can browse the index live. Six tools: `search`, `browse_tree`, `get_summary` (with `tier` = l0/l1/l2 progressive disclosure), `read_file`, `ask`, `get_stats`. Logs to stderr only so stdout stays the protocol channel.
- **Cross-encoder reranking** — the long-stubbed `[retrieval] rerank` flag now does something: a `CrossEncoder` trait with a default `LlmReranker` that listwise-reorders retrieved candidates in one local-model call. Off by default; **fails open** (any model error, empty, or unparseable output falls back to the original order, so it can never make `ask` worse). No new native dependency — an ONNX/`fastembed` cross-encoder stays a future cargo-feature so the default single binary remains ONNX-free.

### Changed

- **Single Send-safe Q&A pipeline** — the CLI, web `api_ask`, and MCP `ask` previously hand-rolled three near-identical retrieval pipelines (a workaround for the old `ask(&Store, …)` being `!Send`). They now all call one `query::answer(db_path, …)` that scopes the SQLite borrow to a synchronous block, so the reranker and the empty-result short-circuit apply uniformly across every surface.

### Fixed

- The empty-result guidance message (run `indexa deep` / `summarize` first) is now consistent across CLI, web, and MCP instead of web-only.

## [0.4.0] — 2026-05-29

The "local context engine" release: Indexa now reads your machine's resources and
works **within** them so a local-AI index no longer freezes the computer, ships a
full Jobs workspace with live AI output, and exposes a one-line abstract tier for
agent-facing progressive disclosure.

### Added

- **Resource engine** — `crates/core/src/resource.rs` detects the machine (RAM, P/E cores, Apple-Silicon unified-memory GPU-wired limit via `sysinfo`), maintains a per-model memory-footprint table, and computes a fit budget. A **memory watchdog** pauses LLM/embedding work when swap pressure rises (the real freeze signal on macOS) and resumes automatically, with a hard 5-minute timeout. Three **resource profiles** (Conservative / Balanced / Performance) via the new `[resource]` config section.
- **`indexa doctor`** — prints detected specs, a per-model peak-memory table, per-mode ETA estimates, and an Ollama env-var check (`OLLAMA_MAX_LOADED_MODELS` / `OLLAMA_NUM_PARALLEL` / `OLLAMA_KEEP_ALIVE`) with the exact `launchctl` commands.
- **Dedicated Jobs tab** — master/detail layout replacing the cramped floating dock: per-job cards, filter pills (All/Running/Done/Failed), a live "what the AI is doing now" panel, expandable/filterable/selectable warnings, an elapsed timer, the summary-queue depth, and a bottom-right status pill.
- **Live AI streaming during summarize** — `describe_stream` / `summarize_dir_stream` emit `LlmFragment` tokens so the Jobs tab shows the model writing each summary in real time (gated on a connected viewer to stay free when unwatched).
- **Tiered summaries (L0/L1/L2)** — every node carries a one-line **abstract** (L0) derived for free from the full summary (L1); raw chunks are L2. Surfaced in export (`<abstract>` / `**Abstract:**` / `"abstract"`), the web `api_summary`, and `indexa describe`.
- **Markdown rendering** in the Ask answer pane (code blocks, inline code, bold, italic, headings, lists) via an XSS-safe renderer.

### Changed

- **`keep_alive` + `num_parallel=1`** sent on every Ollama request so models unload promptly and KV-caches don't multiply — the core of the freeze fix. Single-model-resident discipline with explicit unload on model switch.
- **Calibrated ETA** — the deep dry-run estimate now uses a per-model, prompt-eval-aware throughput model instead of a hardcoded `300 chunks/min`.
- **Filesystem walk prunes build artifacts** — `target/`, `node_modules/`, `.git/`, and caches are no longer descended into (previously classified `Skip` but still indexed), dramatically cutting index size and wasted work.
- **Debounced file watcher** — `watch` now uses `notify-debouncer-full`, coalescing editor save bursts into a single re-index on macOS/Linux (the old poll-interval only affected the fallback poller).
- **In-app confirm modal** replaces blocking native `confirm()` dialogs.
- **Default embedding model** corrected to `nomic-embed-text` (the previous `nomic-embed-text-v1.5` was not a valid Ollama tag).

### Fixed

- **Whole-machine freeze** during `deep`/`summarize` on Apple Silicon — multiple Ollama models staying resident simultaneously crossed the unified-memory swap threshold. The resource engine + `keep_alive` + watchdog prevent it.
- **`indexa ask` panic on non-ASCII content** — context truncation sliced a `String` on a raw byte offset; now walks to a char boundary.
- **Job cancellation** — `DELETE /api/jobs/:id` now actually stops the running job (cancellation flag checked in the deep/summarize/index loops) instead of letting it run invisibly.
- **Worker no longer holds the store mutex across the LLM await**, so web endpoints don't block during background summarization.
- **SSE reliability** — subscribe-before-snapshot eliminates a lost-event race; lagged clients get the terminal Done/Failed re-delivered.
- **DB errors** in `api_tree` / `api_roots` / `api_search` return HTTP 500 instead of masking failures as empty results.
- **`deep --passes`** (silently ignored) removed — passes belong to `summarize`. **Invalid `--mode`** values are rejected instead of silently treated as `augment`.
- **`indexa status`** prints a human-readable UTC datetime instead of a raw epoch.
- **Summarize ETA overflow** when re-running on an already-queued path (total was 0 → garbage ETA); now uses the real pending-queue depth with saturating arithmetic.
- Request **timeouts** on all Ollama HTTP calls (30 s embed, 180 s generate) so a stalled server can't hang a job forever.

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
[0.5.0]: https://github.com/harf-promo/indexa/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/harf-promo/indexa/compare/v0.3.5...v0.4.0
[Unreleased]: https://github.com/harf-promo/indexa/compare/v0.5.0...HEAD
