# Changelog

How Indexa got sharper, release by release — every change that affects what you can do with it.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Local contextualized-chunk embeddings — deterministic contextual prefix (`[describer]
  contextual_prefix`, default off).** The free, local sibling of Anthropic-style Contextual
  Retrieval: at index time each cache-miss chunk's *embed input* is prefixed with its file path,
  section heading, and a capped document-context snippet, so the stored embedding reflects where
  the chunk sits in the whole document — the idea behind Voyage AI's voyage-context-4, realized
  locally with the existing embedder at **zero token cost** (no LLM call, unlike
  `contextual_retrieval`). If both are enabled the LLM path wins. The prefix is applied to the
  embed text **only** — the stored/hashed chunk text is untouched, so the embedding cache and FTS
  index are unaffected. Wired identically in the CLI and web deep paths; new pure helpers
  `build_context_prefix` / `contextual_prefix_texts` in `indexa_query::contextual` (unit-tested).
- **`[chunking] size` / `overlap` are now honored by every parser.** The config existed but was
  dead — all parsers hardcoded 800-word chunks / 100-word overlap. Chunk sizing now threads through
  a defaulted `Parser::parse_chunked` trait method (the public `Parser` API and every external/plugin
  parser are unaffected — the default delegates to `parse`), built once per run via
  `Registry::with_chunk`. Defaults stay **800 / 100**, so an index built without a `[chunking]` block
  is byte-identical to before. `strategy` remains a forward-looking selector (nothing branches on it
  yet); it stays orthogonal to `[describer] contextual_prefix` (boundaries vs. embed input).
- **Dense-mode retrieval eval recipe + manual workflow.** The hermetic CI gate scores sparse-only
  (no Ollama), so it can't measure an embedding change. A new `#[ignore]`-gated
  `dense_rrf_eval_over_golden` test and a `workflow_dispatch`-only `dense-eval` GitHub workflow run
  `indexa eval … --mode rrf` against a populated index (Ollama + `nomic-embed-text`), gating on
  hit_rate/MRR/recall/nDCG so a dense change (contextual-prefix, reranker) is promoted only when
  proven non-regressing. `docs/methodology.md` documents the baseline-vs-branch A/B recipe.
- **Configurable cross-encoder reranker model (`[retrieval] rerank_model`).** The candle DeBERTa-v2
  reranker's model was hardcoded to `mixedbread-ai/mxbai-rerank-xsmall-v1`; it's now a config knob.
  All three mxbai-rerank-**v1** variants share the same architecture, so `base-v1` (~370 MB) and
  `large-v1` (~870 MB) are drop-in higher-quality options. Default stays xsmall (behavior-neutral);
  only applies when `rerank_backend = "cross-encoder"`. (v2/Qwen is a different arch — not supported.)

### Fixed

- **Cross-encoder reranker never actually loaded.** `rerank_backend = "cross-encoder"` silently fell
  open to the LLM reranker for every user who enabled it: candle's DeBERTa loader read the transformer
  at the safetensors root, but HF `DebertaV2ForSequenceClassification` checkpoints nest it under a
  `deberta.` prefix, so loading failed with *"cannot find tensor embeddings.word_embeddings.weight"*
  (swallowed by the fail-open path). Prefixing the base model with `deberta` fixes it; the backend now
  loads and reranks. Surfaced by the new `#[ignore]` load test (no prior test exercised the real model).
- **Markdown/HTML chunk overlap ignored configuration** — `chunk_markdown` hardcoded a 100-word
  overlap even conceptually; it now takes `overlap` from `[chunking]` like every other parser.

## [0.76.0] — 2026-06-28

### Added

- **`add_note` MCP tool (47th tool) — write-back learning loop.** An AI caller that learns
  something new (a bug root-cause, a design decision, a meeting outcome) can now persist it
  as a Markdown note in the Indexa data directory: `add_note(pack, title, body)`. The note
  is written to `<data_dir>/notes/<slug>-<sha8>.md`, attached to an existing pack, and
  immediately indexed via `indexa index <notes_dir>` — so it becomes searchable via `search`,
  `ask`, and `export_pack` within the same session. Re-submitting the same title + body is
  idempotent (same file name → in-place overwrite). Notes inherit secret redaction on
  `export_pack` automatically. Requires a pre-existing pack (`create_pack` first). Implemented
  in `indexa_core::notes` (shared by MCP and any future CLI surface). Inspired by the
  `memodb-io/acontext` write-back pattern: "context that an agent writes to is context that
  gets better over time."

- **Catalog retrieval mode — progressive disclosure.** `ask(catalog: true)` runs the full
  retrieval pipeline (hybrid + boosts + rerank + MMR) but returns only a scored file list
  with L0 one-line abstracts — no chunk bodies, no local LLM synthesis. Use it as the "table
  of contents" step: ask the catalog, pick the interesting files, then expand them with
  `get_summary`/`read_file`/`get_chunk_context` and synthesize with your own (stronger) model.
  This is the cheapest retrieval mode: minimal tokens, bounded KV-cache, zero local model
  calls. Compatible with `session_id` (conversational follow-ups rewrite the search query
  before retrieval). Incompatible with `agentic` (catalog is a single-pass retrieval, not a
  multi-hop loop). Usage is recorded as `ask_catalog` in the savings ledger. Optional param
  on the existing `ask` tool — `catalog` absent ⇒ byte-identical behavior. MCP tool count
  stays 47.

- **`indexa_core::notes` module** — `slugify(s)` and `write_note_file(data_dir, pack, title,
  body)` as public, tested, reusable primitives. Future surfaces (CLI `indexa note add`,
  web sidebar) can call the same code without duplicating the cache-as-file logic.

- **Privacy: safe-by-default sensitive-path deny list + index-time redaction (#335).** A new
  `DeepScanPolicy::Sensitive` classification blocks descent into credential/key stores — `.ssh`,
  `.gnupg`, `.aws`, macOS Keychains, Chrome/Brave/Firefox/Safari browser profiles, `.mozilla`,
  `.password-store`, and 1Password group containers (11 curated hints). The walker prunes them via
  `WalkState::Skip` unless a caller opts in with `[scan] include_sensitive = true` (default `false`).
  Independently, `[scan] redact_at_index` (default `true`) runs `redact_secrets` on every chunk before
  it enters the store, so API keys / tokens / PEM blocks in ordinary source files never become
  full-text-searchable. Summarization also skips `Sensitive` entries (no LLM cost on credential dirs).
  Prerequisite for any machine-wide (`~/` or `/`) scan — the whole-computer-indexing groundwork.

### Fixed

- **Stop Indexa from filling the disk — five root-cause fixes (#337).**
  - **Walker** — replaced `jwalk` + a root-only `.gitignore` parser with `ignore::WalkBuilder`
    (ripgrep's parallel walker). Nested `.gitignore` files are now honoured, and `WalkState::Skip`
    prunes `.git/`, `node_modules/`, `target/`, `__pycache__`, and 10+ other junk dirs *before*
    descent (previously 129k+ junk entries entered the index). New scan-time 8 MB per-file cap
    skips large blobs. `jwalk` dropped (the `ignore` crate was already in the tree).
  - **WAL** — `wal_autocheckpoint=1000` + `Store::checkpoint_truncate()` on every `Store::open`
    (the WAL grew past 100 MB and never truncated between runs).
  - **VACUUM** — `Store::vacuum()` + `indexa prune --vacuum` (prints before→after size); no VACUUM
    ever ran before.
  - **Logs** — cap the log directory to 14 daily files (`max_log_files(14)`), previously unbounded.
  - **Large-root guard** — `is_huge_root()` detects `/` and `$HOME`; interactive sessions prompt
    `[y/N]`, non-tty sessions bail with a clear error. `--yes` on `scan`/`index`/`deep` bypasses
    for scripted use.

## [0.75.0] — 2026-06-23

### Added

- **Token estimate in the savings readout.** Every surface that shows "% less to your AI tool"
  now also shows the concrete token estimate: `(~N tokens at ≈4 bytes/token)`. The per-answer
  impact chip, the running conversation badge, and the CLI impact line all use the same integer
  formula as the weekly Impact dashboard — one formula, no drift between views. `human_count()`
  promoted from a private function to `pub fn` in `indexa_core::text` (single source of truth).

- **"How is this measured?" explanation.** The per-answer impact chip and the weekly Impact
  dashboard both now show a `<details>` disclosure explaining what "Served" and "Source" mean
  (Served = answer text + snippets delivered; Source = on-disk size of cited files only, not
  the whole repo), so the savings number is auditable without trusting a black box.

- **Map SVGs fill their panel.** The Graph and Treemap SVGs now grow to fill whatever space the
  panel has via `<div class="graph-canvas-wrap">` / `<div class="treemap-canvas-wrap">` wrappers
  (`flex:1`, `position:relative`; SVG is `position:absolute; inset:0`), replacing the old
  `calc(100vh - 300px)` / `calc(100vh - 280px)` magic numbers. Legend and note elements remain
  visible below the canvas at all viewport heights.

### Fixed

- **Summary-header button pile-up on narrow workspaces.** The three action buttons (Ask about /
  Regenerate / Export) now live in a `.summary-actions` group that wraps to a second line instead
  of overflowing or stacking on top of the title.

- **Folder-row action buttons overlapping the folder name.** The hover-strip fade starts 8 px
  wider (`padding-left: 28px`) so the folder name is visibly readable before it transitions into
  the action buttons.

- **Topbar version label no longer overflows.** `.app-version` now has `flex-shrink:1; min-width:0;
  text-overflow:ellipsis` so the version string truncates gracefully at narrow widths.

- **Engine bar progressive hide on narrow viewports.** Non-critical widgets shed at three
  breakpoints (1024/860/720 px) so RAM, pressure, and the Activity button are always reachable.

- **Jobs drawer stacks on narrow viewports.** The master/detail split becomes a vertical stack
  (`flex-direction:column`) below 768 px.

- **Add-Root modal unified onto the `.open` class pattern.** `openAddRoot` now uses
  `classList.add('open')` (CSS: `.modal-overlay.open { display:flex }`) instead of
  `modal.style.display = 'flex'` — consistent with every other overlay in the app.

### Changed

- **Scrim token hygiene.** Six hardcoded `rgba(0,0,0,…)` backdrops replaced by `var(--scrim)`.
  Mobile sidebar z-index values updated to the design-token stack
  (`--z-drawer:150` / `--z-overlay:100`), fixing a hazard where dropdowns could punch through.

## [0.74.0] — 2026-06-23

### Added

- **Monochrome macOS menu-bar tray icon.** The Tauri tray now loads a dedicated
  black-on-transparent template PNG (`apps/indexa-desktop/icons/tray-mono.png`) and sets
  `icon_as_template(true)` — macOS auto-inverts it to white on a dark menu bar and dark on a
  light one, matching native system icons. The Dock icon, window icon, browser favicon, and
  in-app apostrophe marks all stay brand-green. No-op on Windows/Linux.

### Changed

- **Add-Root modal now traps focus and accepts Enter to submit.** `openAddRoot` activates the
  same `inert`-background focus trap the side drawers use (`setBackgroundInert(true)`), so Tab
  key stays inside the modal while it's open. Enter in the path field triggers the index
  immediately. Focus returns to the opener element on close.

- **Export menu is now fully keyboard-navigable.** ArrowDown/ArrowUp move focus between export
  options, Enter/Space activate the focused item, Esc closes the menu and returns focus to the
  trigger button, Tab closes without activation — completing the WAI-ARIA menu keyboard pattern
  (the aria attributes shipped in v0.73's PR #322; the roving-focus behaviour ships here).
  A module-level cleanup reference prevents listener accumulation when the menu is opened
  multiple times.

## [0.73.0] — 2026-06-22

### Added

- **Running per-conversation savings in the chat.** The core pitch ("Indexa serves a slice, not
  whole files — saving paid tokens") was shown per-answer + in the Settings impact panel; now a quiet
  strip below the chat thread shows the **conversation's running total** ("This conversation: served
  X vs Y of source — N% less, across M answers"), refreshed after each turn from the authoritative
  `GET /api/session-impact/{id}` ledger. Read-only, fail-open (hides on any error / no meaningful
  saving), cleared on "＋ New". No backend or MCP change.

### Changed

- **Internal: graph overlay layers de-duplicated.** The semantic/category/pack overlay blocks in
  `crates/web/src/handlers/graph.rs` (near-identical want-flag → fresh-conn `spawn_blocking` →
  fail-open → `EdgeDto` pattern) now share one `overlay_layer` helper — single source of truth for the
  fresh-connection + fail-open + cost-guard discipline, so a future layer can't silently skip it.
  Behavior-identical (verified: `?layers=semantic|category|pack|communities` + combined + no-layers
  all unchanged).

### Added

- **Map "Communities" view — clustering + hubs + surprising connections (`?layers=communities`,
  default-off).** The biggest knowledge-graph UX gap: an opt-in overlay that groups files into
  communities via **Louvain** (greedy modularity, `Store::detect_communities` in
  `crates/core/src/store/communities.rs` — deterministic, dependency-free; Louvain over plain label
  propagation so two cliques joined by a weak link don't collapse into one "monster community"),
  tints nodes by community (≤6 low-saturation HSL tints scoped to the SVG data layer; the tail merges
  to neutral grey — green/teal/info stay reserved for UI state), surfaces each community's **hub**
  (highest-PageRank file), and highlights **bridge edges** that cross communities ("surprising
  connections"). Computed server-side over the *structural* call graph (overlay edges don't shift
  membership), inline + fail-open; the legend lists clusters by hub with an honest "approximate"
  caveat. Default-off ⇒ the plain graph is byte-identical. (Verified live: 5 clusters + 29 bridges
  on Indexa's own index.)

- **"Same pack" knowledge-graph layer (`GET /api/graph?layers=pack`, default-off).** The third Map
  overlay after v0.70 "Related by meaning" (semantic) and v0.71 "Same category": files the user put
  in the same Context Pack are linked via a deterministic **star per pack** (O(n), not an O(n²)
  clique) — `Store::pack_file_edges` (`crates/core/src/store/pack_edges.rs`), fresh-connection,
  fail-open, no schema. Exact (user curation), not heuristic. A "Same pack" toolbar toggle + legend
  row; combine with the others (`?layers=semantic,category,pack`). No `layers` ⇒ byte-identical.
- **`indexa multimodal` — one-command multimodal readiness + enable.** The multimodal parsers (image
  captioning, PDF OCR, audio transcription, video-frame captioning) are fully built but opt-in;
  `indexa multimodal` now reports which are **ready** on this machine (detects tesseract/pdftoppm,
  ffmpeg, whisper-cli on PATH + a vision model in Ollama) and the exact `[parsers.*]` flag each needs,
  and `indexa multimodal --enable` turns on every ready feature via the **safe** config round-trip
  (`config::load` → mutate → `config::save`; refuses to overwrite an unparseable config — the v0.69
  anti-wipe guard; written 0600). The same readiness report is now a section in `indexa doctor`.

## [0.71.0] — 2026-06-21

### Added

- **Brand identity: a real logo + app icon (was a placeholder).** Indexa now has a mark built from
  the design system's own signature — the **green Harf apostrophe `#A4CD39` on an ink ground** (sharp,
  rounded-rect for the Dock). A single `crates/web/assets/ui/favicon.svg` is the source of truth:
  served as the browser **favicon** (`GET /favicon.svg`, was absent), regenerated into the full
  desktop **app-icon set** via `tauri icon` (so the installed macOS app, Dock, and tray show it
  instead of a blank placeholder — `icon.icns`/`.ico`/PNGs/Store logos), and shipped as a README
  lockup (`docs/assets/logo.svg`). The web header drops the generic `⊡` glyph for the apostrophe mark
  + a grey **"indexa"** wordmark (green-as-punctuation, matching the "by Harf" footer).
- **Knowledge-graph "Same category" layer (Track 3, `GET /api/graph?layers=category`, default-off).**
  A second opt-in Map overlay beside the v0.70 "Related by meaning" layer: files the user classified
  into the same category are grouped via a deterministic **star per category** (O(n), not an O(n²)
  clique) — `Store::category_file_edges` (`crates/core/src/store/category_edges.rs`), computed on a
  fresh connection, fail-open, no schema. Dashed-grey "grouping" edges (never a UI-state colour), a
  "Same category" toolbar toggle + legend row shown only when present. Combine with `semantic`
  (`?layers=semantic,category`). No `layers` ⇒ byte-identical to the call-graph view.

## [0.70.0] — 2026-06-21

### Added

- **GraphRAG "Approach C" — topic-clustered synthesis context (`[retrieval] graphrag_clusters`,
  opt-in, default-off).** On a **broad, unscoped** question, the retrieved hits are grouped into a
  few semantic clusters (greedy cosine-threshold agglomeration over the chunk embeddings MMR already
  fetches — no extra DB cost) and the synthesis prompt presents them under `=== THEME … ===` headers,
  so the model can structure a coherent multi-faceted answer. A second sub-flag `graphrag_summarize`
  adds a one-line LLM theme per cluster (≤ `graphrag_max_clusters` extra calls, fail-open). Tunables:
  `graphrag_max_clusters` (4), `graphrag_cluster_sim` (0.55). **Safety:** the clustered packing is a
  pure regrouping — citations keep a single global `[1..N]` counter (no dangling citation), the
  off/no-cluster path is **byte-identical** to today (asserted by tests), and clustering **fails open**
  to flat packing. Retrieval ranking is untouched, so the hermetic `indexa eval` is unaffected.
  **Honest A/B (live, Ollama, on Indexa's own index):** like the per-file cap, broad-question hit
  pools here are already topically cohesive, so at the default threshold they collapse into ~1 cluster
  (no distinct themes) and answers match the flat baseline — so it ships **default-off and unpromoted**
  per the pre-registered rule (don't ship a weak proxy on). It's a real lever for corpora where a broad
  query spans genuinely distinct topics. A/B harness: `cargo test -p indexa-query graphrag_ab -- --ignored`.
- **Knowledge-graph upgrade: a "Related by meaning" layer on the Map (Track 3, opt-in, default-off).**
  The Map's force-directed graph linked files that *call* each other; a new toggle overlays
  **semantic edges** between files with *similar content* — a topic relationship the call graph
  can't see — turning the call graph into a navigable knowledge graph. Edges are derived at request
  time from `chunks.embedding` (per-file centroid → pairwise cosine ≥ threshold; **no schema, no
  persistence**, re-derivable). Backend: `Store::semantic_file_edges` (`crates/core/src/store/semantic_edges.rs`,
  reusing the `search.rs` cosine/blob/scan helpers) + `GET /api/graph?layers=semantic` (`sim_threshold`
  default 0.78, `sim_max_nodes` default 300 bounds the O(n²) pass), computed on a **fresh connection**
  off the shared mutex and **fail-open** (an error leaves the call graph intact). Frontend: a "Related
  by meaning" toolbar checkbox (`js/28-graph-layers.js` + `css/20-graph-layers.css`, info-blue dotted
  edges, distinct from call tiers; a legend row shown only when semantic edges exist). **Default-off:**
  no `layers` param ⇒ the response and render are byte-identical to the call-graph-only view. Category-
  and pack-membership layers are a documented follow-up.

- **Happy-path multimodal e2e tests (the first real-media coverage).** Image captioning, PDF OCR,
  audio transcription, and video-frame extraction were all wired and gracefully degrading, but only
  their *error* paths were tested. New `--ignored`-gated tests exercise the REAL external tools on
  committed fixtures — `crates/parsers/tests/multimodal_live.rs` (OCR via tesseract+poppler,
  transcription via whisper-cli, frames via ffmpeg) and `crates/llm/tests/caption_live.rs` (vision
  captioning via Ollama). They skip cleanly when a tool/model is absent, so plain `cargo test` and CI
  stay green; run them with the tools installed to confirm the integrations end-to-end. All four
  paths verified live on this release. **Vision-model note:** the default caption model `gemma3:4b`
  loads on a stock Ollama; some Ollama builds can't load `llama3.2-vision` ("unknown model
  architecture: `mllama`") — use `[parsers.image] model = "moondream"` (or the default) if so.

- **MCP/CLI/web "retrieval-only" Ask + synthesis-model transparency.** When another tool calls the
  Indexa MCP `ask`, the answer is synthesized by Indexa's **local** model (e.g. `ollama/gemma3:12b`),
  never the caller's model — and most other tools (`search`/`get_summary`/`read_file`/`export_pack`/…)
  are pure retrieval. Now `ask` accepts `synthesize: false` (MCP), `--no-synthesize` (CLI), and
  `"synthesize": false` (web `POST /api/ask`): Indexa runs its **full retrieval pipeline** (hybrid +
  boosts + rerank + MMR + per-file cap + project overview + coverage) and returns the **packed context
  slice** for the caller to answer with its own — typically stronger — model, at no local-model cost.
  Synthesized answers now also report which local model produced them (`Answer.model`, e.g.
  `ollama/gemma3:12b`; CLI prints it, web/SSE echo it, the MCP tool appends it) so an agent knows the
  answer is bounded by that model, not its own. Additive — the MCP **tool count stays 46** (optional
  param), the single-shot synthesis path is byte-identical, and retrieval-only never persists a
  conversation turn (a slice is not an answer). The MCP `ask` description + server instructions now
  steer capable callers toward `synthesize: false`.
- **Opt-in per-file cap for broad questions (`[retrieval] broad_per_file_cap`).** On a broad/thematic
  question with no `scope`, caps how many chunks one file may contribute to the retrieved pool before
  other files get a turn, so a single chunk-dense file can't monopolise the answer's context. It's a
  pure reorder that never drops a hit (overflow just moves to the budget tail). **Default `0` (off);**
  focused and scoped `ask`s are unaffected by construction. Honest note: on a file-diverse corpus
  (e.g. Indexa's own code, where a live A/B showed broad-question retrieval already spreads ~1 chunk
  per file) there's nothing to balance, so it's a no-op there — it exists for corpora where one large
  file dominates retrieval. The deeper thematic-synthesis "GraphRAG" (cluster → per-cluster summarise
  → combine) remains future work.

## [0.69.1] — 2026-06-21

### Fixed

- **Critical: v0.69.0 could not open an existing (pre-v0.69) index.** The per-session-ledger
  migration (#293) created `idx_tool_usage_session ON tool_usage(session_id)` in the base schema
  block, which runs before the `ALTER TABLE … ADD COLUMN session_id` migration. On any index built
  by v0.68 or earlier — where `tool_usage` already exists without that column — opening it failed
  with `no such column: session_id`, so every CLI/web/MCP command aborted. The index is now created
  (idempotently) only after the column is guaranteed to exist, migrating older indexes in place.
  Fresh-DB tests didn't catch it (a new in-memory DB gets the column from the base `CREATE TABLE`);
  a regression test now opens a simulated pre-v0.69 index. **Upgrade straight to v0.69.1.**

## [0.69.0] — 2026-06-21

### Added

- **Per-session token-savings ledger.** Each Conversational-Ask session now persists its own
  cumulative savings, exposed at `GET /api/session-impact/{session_id}` — so a conversation can show
  how much it saved versus serving whole files (previously only the weekly `/api/impact` aggregate
  existed). A migration-guarded nullable `session_id` column on `tool_usage` tags web `ask` rows with
  their session; MCP/CLI calls stay session-less and still feed the weekly aggregate. Reuses the
  existing `ask_sessions` infrastructure; the token figure is the same honest `bytes/4` estimate.
- **C and C++ in the code graph.** `dependencies` / `who_imports` / `who_calls` / `blast_radius` /
  `code_graph` now cover C and C++ (8 languages, up from 6). Calls resolve through `call_expression`
  (including C++ method and `ns::`-qualified calls); imports through `#include` (`<system>` and
  `"local"` headers); function definitions link cross-file. `#include` matching is lexical/loose, so
  C/C++ leans on the labeled bare-name fallback tier more than the others (documented in
  `docs/methodology.md`). Adds `tree-sitter-c` + `tree-sitter-cpp` (MIT, bundled-C via `cc` like the
  other grammars — openssl-free preserved on host and aarch64-linux). (Track 2, retrieval intelligence.)
- **Batched cloud embedding (OpenAI / Google).** When `[embedding] provider` is `openai` or
  `google`, the deep phase now embeds each ~64-chunk group in **one** request (OpenAI array `input`;
  Google `:batchEmbedContents`) instead of one HTTP round-trip per chunk — a large speedup for cloud
  embedding on big indexes (the local Ollama adapter already batched). Both **fall open**: any error,
  count/dimension mismatch, or — for OpenAI — a response whose `index` fields don't realign cleanly
  drops back to per-chunk embedding, so a batch can never misalign or lose a file's vectors. The
  realignment logic is unit-tested; the live round-trip is covered by `#[ignore]`d API tests.

### Fixed

- **`POST /api/keys` no longer wipes your config when it can't be parsed.** The handler loaded the
  config with `unwrap_or_default()`, so a present-but-unparseable `config.toml` silently became a
  fresh default that was then saved back — clobbering every setting and any already-stored API keys
  just to add one. It now refuses (HTTP 500) to overwrite an unparseable config, matching the other
  config-writing endpoints; a missing config still loads as default, so first-time key entry works.
- **Chunkers can't hang on a degenerate config.** The text fixed-window loop (stride =
  `chunk_size − overlap`) and the markdown per-section splitter (stride = `chunk_size − 100`) could
  advance by zero — and spin forever — when `overlap >= chunk_size` or `chunk_size <= 100`. Both now
  `.max(1)` the stride (a no-op at the default `size = 800` / `overlap = 100`), matching the existing
  guard in the org-mode chunker. Regression tests added.
- **Secret redaction preserves the assignment separator.** `redact_secrets` rewrote every
  `key: value` / `key=value` to `key = [REDACTED-secret]`, normalizing the separator; redacting a
  YAML/TOML config therefore broke its syntax. It now keeps the original separator
  (`api_key: [REDACTED-secret]`, `token=[REDACTED-secret]`), so the redacted artifact stays
  syntactically intact. The secret value is still fully removed.

### Documentation

- **Trued-up retrieval & parser docs that had drifted from the code.** `docs/config.md` now shows
  the real `[retrieval]` defaults (`top_k = 12`, `rerank = true`, `context_budget = 8000` — they
  flipped in v0.44) and documents the previously-undocumented `rerank_backend`, `mmr_lambda`,
  `recency_boost`/`recency_days`, and `archive_segments`/`archive_penalty` knobs. Image captioning,
  audio transcription, and PDF OCR are documented as **opt-in and shipped** (with their real config
  keys and external-tool requirements) instead of "(future) / not yet wired". `docs/methodology.md`
  now describes re-ranking as on-by-default with the v0.43 cross-encoder backend as the opt-in
  option (was "off by default, a future cross-encoder"), fixes the invalid `[parsers.pdf] backend =
  "marker"` example to `"ocr"`, and adds a decision-log row for the v0.44 default flip. The
  `indexa graph` empty-result hint now lists all eight code-graph languages (C/C++ were missing).
- **Synced the remaining stale docs surfaced by a full-codebase audit.** `USAGE.md` now states the
  MCP server exposes **46 tools** (was "42") and shows the real `[retrieval]` defaults (`top_k = 12`,
  `rerank = true` + `rerank_backend`, `context_budget = 8000`); the `docs/config.md` "Full example"
  retrieval block was corrected to match; the `Describer::describe` trait doc now says "1–2 sentence"
  (matching the prompt templates) instead of "One-sentence". Also dropped two unused dependencies
  (`thiserror`, `tracing`) from `crates/parsers/Cargo.toml` (kept at the workspace level for the
  crates that do use them).

## [0.68.0] — 2026-06-20

### Changed

- **Sparse / keyword search now tokenizes the query instead of phrase-matching it whole.** The FTS5
  MATCH was built by wrapping the entire query in quotes — one phrase — so a multi-word natural-
  language question (`"how does the watcher reindex changed files"`) only matched a near-verbatim
  adjacent run and returned almost nothing in `--mode sparse`. It now emits `"<whole query>" OR
  "term1" OR "term2" …`: the exact phrase still scores highest (adjacency), while the individual
  content terms (stopwords dropped) add recall, BM25-ranked. The same expression feeds the lexical
  arm of the default hybrid (`rrf`) `ask`/`search`, so they gain the recall too. On Indexa's own
  self-golden eval (sparse): hit-rate 0.69 → **1.00**, recall 0.67 → **0.98**, nDCG 0.67 → **0.85**
  (the 18 existing keyword probes unchanged; 8 added natural-language questions go miss → hit). No
  new dependency. *(Track 2, retrieval intelligence — PR #1.)*

- **Web UI restyled onto the Harf design system** (a bilingual editorial brand: cool-grey
  scale + a single green apostrophe as punctuation, Geist type, √2 spacing, 5/4 type scale, sharp
  corners + hairlines). **Stage 1 — the token foundation:** `01-tokens.css` is rewritten to the Harf
  palette/type/spacing/radii/motion with `light-dark()` dark mode (the existing `data-theme` toggle
  still drives it); the legacy token names alias onto Harf tokens, so the whole UI re-skins. Active/
  selected UI is now teal (`--positive`); the brand green is reserved for marks. Fonts switch to Geist
  + Geist Mono. **Stages 2 & 4 — components + brand:** cards/panels/modals are now sharp-cornered with
  hairline borders (4px kept only on buttons/inputs); primary buttons use the ink fill (paper text,
  inverts per theme) while teal stays for active/selected state; the focus ring is Harf's green ring;
  leftover GitHub-blue selection tints became the teal accent; floating layers regained their shadow (a
  `light-dark()`-wrapping bug from stage 1 that made every shadow resolve to `none` is fixed); and a
  quiet "by Harf" sign-off (a leading green apostrophe stamp) sits in the footer. **Stage 3 — emoji →
  marks:** the ~70 emoji across the UI are replaced with the system's vocabulary — mono status marks
  (`✓` `✕` `⚠`) inline, and clean single-stroke inline SVG icons (currentColor) for the topbar/footer
  buttons and the theme toggle. The UI now reads end-to-end as a Harf product in both light and dark.
- **Answer "confidence" is now labelled "retrieval coverage"** across CLI (`ask`), web, MCP, README, and
  `docs/methodology.md`. It was always a heuristic read of the retrieval evidence — not a calibrated
  probability — so the honest term is "retrieval coverage" (how well the cited sources cover the
  question), not "confidence." Display-only: the `--json` / MCP / SSE field stays named `confidence`
  for backward compatibility.

### Added

- **`indexa deep --no-embed`** — an FTS-only deep pass: parse + chunk + index for sparse (BM25)
  search **without computing embeddings**. Skips the Ollama preflight and every model call
  (embeddings, contextual blurbs, image captions, audio transcription, PDF OCR), so it runs fully
  offline with no models pulled. The index serves `--mode sparse` search/`ask`; dense/hybrid
  retrieval needs a later plain `deep` (which self-heals the vector-less chunks automatically). Makes
  a retrieval-eval run hermetic, and is handy for a quick models-free index.
- **Indexa now runs its own retrieval eval in CI.** A new advisory `retrieval eval (self-golden,
  hermetic)` job indexes the checkout with `deep --no-embed` and scores the **exact production
  `retrieve()` ranking** against a committed golden set ([`fixtures/self-golden.json`](fixtures/self-golden.json))
  — catching chunking / parsing / tokenization / ranking regressions that fmt/clippy/test can't see.
  Golden `expect_paths` now also accept repo-**relative** paths (boundary-suffix matched against the
  stored absolute path), so a committed fixture is portable across checkouts and machines.
- **`indexa eval` now reports recall@k and nDCG@k** alongside hit-rate / MRR / precision (per-question
  columns + run aggregates `mean_recall` / `mean_ndcg`, in the table and `--json`). `hit@k` only asks
  "any expected path?"; **recall@k** grades how many of them were retrieved, and **nDCG@k** catches a
  *ranking* regression (an expected hit sliding from #1 to #6) that hit-rate is blind to.
- **`indexa eval` gained a baseline regression gate.** `--baseline <prev.json>` prints the per-metric
  delta vs a saved run (a `vs baseline:` line); `--max-regression <d>` (default `0.0`) exits 1 if any
  aggregate metric drops more than `d` below it. This turns the eval from an absolute floor into a
  relative gate — a retrieval change can prove it didn't regress ("MRR 0.74 → 0.61" fails the build).
  Float round-trip jitter is ignored, so an identical run never false-fails.

## [0.67.0] — 2026-06-20

A hardening, parity, and performance pass — plus a dependency refresh. Found and verified through
three adversarial review sweeps of the codebase.

### Security

- **Context Pack export over the web no longer leaks secrets.** `GET /api/packs/{name}/export` was
  the one export surface that served indexed content without the secret-redaction every other export
  path applies; an API key that had slipped into a summary or chunk could be served verbatim. It now
  redacts like the rest, with a regression test guarding it.

### Added

- **Sharper MCP/web/CLI parity.** `search`/`search_pack` now show each hit's chunk `#seq` (so an
  agent can drill in with `get_chunk_context`); `export_pack` gained `changed_since`/`category`
  slicing (matching CLI/web); `code_graph` can report dependency **cycles**; `ask` shows the
  per-answer "retrieved-slice" impact to the agent (as the CLI and web already do); `ask` accepts a
  `top_k` retrieval-breadth override on MCP and web; and `read_file` accepts a byte `offset` so an
  agent can page past the 40 KB cap on large files.
- **Web Insights gained the `largest` (bloat) and `languages` (breakdown) views** that the CLI and
  MCP already had.
- **Configurable archive down-weighting.** New `[retrieval] archive_segments` and `archive_penalty`
  let you extend the historical-path list (e.g. `legacy/`, `attic/`, `backup/`) or disable the
  penalty entirely (`archive_penalty = 0.0`) — fixing the case where a live working folder named
  `old/` was silently down-ranked with no opt-out. Defaults reproduce the previous behavior.

### Changed

- **Dependency refresh:** `zip` 2→8, `kamadak-exif` 0.5→0.6, `notify` 6→8 + `notify-debouncer-full`
  0.3→0.7, `axum` 0.7→0.8, and `actions/setup-node` 5→6. No behavior change; the tree stays
  openssl-free.
- **Faster tree browsing.** Expanding a directory previously ran several per-child subtree scans and
  held the shared store lock for the whole query; it now uses one set-based aggregation pass and a
  fresh read connection, so a big folder no longer stalls other requests. Behavior is unchanged
  (proven by an equivalence test against the previous implementation).
- **Conversational follow-ups are more robust** — the query-rewrite step no longer mistakes a chatty
  model preamble ("Sure, here's the query:") for the actual search query.

### Fixed

- **`indexa watch` correctly indexes newly-created files.** A new file under a watched root used to
  get its chunks indexed but no entry row, so it was never summarized and was silently dropped by the
  next `prune`. It now gets a proper entry (and editing a file no longer wipes its classification).
- **No more dangling `[N]` citations** in answers whose last source was trimmed to fit the budget —
  the trimmed chunk is now cited like the rest.
- **No panic on odd interval input.** `--changed-since`, the `?changed_since=` web param, and the
  `[scan] auto_reindex` config no longer crash on a value ending in a multibyte character; they
  reject it cleanly.
- **The "update your CLI" banner clears once you do.** Running `indexa update` now clears the stale
  CLI-version marker, so the web banner stops nagging about an already-applied fix.
- **Honest readouts.** `prune` (CLI + MCP) now counts the app-detections it removes; the docs no
  longer overstate the code graph as covering C/C++ (it's six languages: Rust, Python, JS, TS, Go,
  Java).
- **Remote-source fetches are size-capped** (8 MB) so a hostile or huge page can't exhaust memory.
- Internal hardening: config files are tested to be `0600`, a flaky ANN-recall test was de-flaked,
  unsupported `**` fingerprint markers are correctly rejected, and duplicated escaping / char-boundary
  helpers were consolidated into one tested place.

## [0.66.0] — 2026-06-19

Indexa now understands *groups* of files, not just individual ones — it recognizes when a directory
is a known application or structure (a Rust crate, a Next.js app, a Django project, a macOS `.app`
bundle, a Terraform module, a Jupyter project, …) and folds that into the index's knowledge.

### Added

- **Application & structure recognition.** A directory is now matched against a signature library to
  identify what kind of thing it is, across four families: code/frameworks (incl. CMS like
  WordPress), OS software & bundles (macOS `.app`, Snap, Homebrew, …), infra & config (Docker,
  Kubernetes, Terraform, Ansible, GitHub Actions, …), and document/data bundles (LaTeX, Quarto,
  Jupyter, datasets). Detection runs automatically during `indexa index` and is **persisted**, so it
  surfaces everywhere: `indexa inspect` (an "App" line), the **project overview** that `ask` uses for
  broad questions (so an answer can say "this folder is a Django app"), MCP `inspect`/`project_overview`,
  and the web "Indexed facts" panel. `indexa fingerprint` now reads the persisted detections.
- **Richer signature grammar.** Fingerprints can now require `all_of`/`any_of` markers, forbid
  `none_of` anti-markers (e.g. a Terraform module but not a `.terraform/` cache), and match nested
  paths (`Contents/Info.plist`) and globs (`*.xcodeproj`, `*.tf`) — not just direct-child filenames.
  Overlapping matches are ranked by specificity (Next.js wins over a bare Node package). The user
  `fingerprints.json` catalog still works; old definitions parse unchanged.
- **Sourced signature library.** A broad curated built-in library plus a snapshot seeded offline from
  CycloneDX cdxgen's project-type catalog (Apache-2.0, with per-rule provenance) for the long tail of
  language manifests. The runtime never fetches anything — the snapshot is committed and regenerated
  by a maintainer-run generator (`tools/gen-fingerprints`).

## [0.65.0] — 2026-06-19

The update window respects your history — and now your terminal does too. When the desktop app
updates but the standalone CLI it spawns (and the MCP server behind it) stays behind, that skew is
no longer silent: Indexa detects it and tells you how to fix it.

### Added

- **CLI ↔ app version-skew detection.** `indexa doctor` gains a **Version sync** check and
  `indexa status` (human + `--json`) reports `app_version`/`version_skew` when your terminal
  `indexa` is older than the installed desktop app — the trap where the app updated but the CLI
  (and the MCP server that runs `indexa mcp`) kept serving stale behavior with no signal. The MCP
  `get_stats` tool warns an agent when it's talking to a stale binary. Each tells you the fix
  (`indexa update`, then restart MCP). Fail-open and offline; the tool count stays 46.
- **Stale-CLI banner in the web UI.** After a desktop update whose CLI auto-refresh didn't land the
  new version, the app records it and the web UI shows a dismissible banner (driven by
  `GET /api/health`'s new `cli_skew` field).

### Fixed

- **The post-update CLI refresh is no longer silent best-effort.** The desktop app now verifies the
  refreshed CLI actually reports the new version and surfaces the result instead of swallowing a
  failed download/sign — the root cause of a CLI/MCP that quietly rotted several versions behind the
  app. A failed `codesign` on a freshly-installed CLI is now logged rather than discarded.

## [0.64.0] — 2026-06-18

Conversational Ask, an MCP Resources + Prompts surface, structured answers, and two
feature-completeness fixes — all additive (single-shot Ask is unchanged).

### Added

- **Conversational Ask (multi-turn).** Pass a `session_id` and follow-ups remember the
  conversation: prior turns are folded into the prompt (budget-clamped to ≤25% of the context
  budget, oldest-first) and the follow-up is rewritten into a standalone retrieval query (one
  extra local-LLM call, only when there's history — single-shot Ask gains zero latency). Wired
  across the **web chat** (a persistent thread + a "＋ New conversation" button), the **CLI**
  (`indexa ask --session-id <id>` / `--continue`, with a `--json` `session_id` field), and the
  **MCP `ask` tool** (optional `session_id`). Sessions persist in two new tables
  (`ask_sessions`, `conversation_turns`).
- **MCP Resources + Prompts.** The MCP server (previously tools-only) now also exposes **4
  read-only resources** — `indexa://overview`, `indexa://packs`, `indexa://pack/{name}`,
  `indexa://summary/{path}` (secrets redacted) — and **3 prompt templates**:
  `onboarding-overview`, `explain-file`, `pack-context`. The 46-tool surface is unchanged.
- **Markdown tables in answers.** The web chat renderer now draws GFM pipe tables, and the
  answer prompt gently suggests a table when comparing several items.
- **Answer coverage hint.** Ask now reports salient question terms that appear in *none* of the
  cited sources ("may not cover: …") in the CLI, web, and MCP — a heuristic "the index may not
  cover this aspect" signal (previously an always-empty placeholder).

### Fixed

- **Speaker-note ↔ slide mapping in `.pptx`** now follows the relationships graph
  (`ppt/slides/_rels/slideN.xml.rels`) instead of ordinal position, so notes attach to the right
  slide even when only some slides have them (was off-by-one with sparse notes).

## [0.63.0] — 2026-06-18

Maintainer-facing internals cleanup — no behavior change.

### Changed

- **Split `decisions/detectors.rs` (2,009 lines) into `detectors/{mod, tests}.rs`.** The 1,069-line
  inline test module moved to its own file; the production code is byte-identical and stays in `mod.rs`.
  A pure move with zero re-wiring (the test module keeps the same module path, so its `super::`
  references are unchanged), proven by the 32 detector tests passing unchanged.

## [0.62.0] — 2026-06-18

Catch a slow Ollama before it stalls an index.

### Added

- **`indexa doctor --latency`** — times a tiny embed + generate against Ollama and reports each
  (e.g. `embedding (nomic-embed-text): 820 ms · generation (gemma3:4b): 3800 ms`), warning when
  either is slow. `doctor` already confirmed the models were *pulled*; this catches the case where
  they're present but the server is overloaded, swapping, or a slow/remote host — so you find out in
  one second instead of ten minutes into an index. Opt-in (it loads the models); plain `indexa doctor`
  stays token-free and now points to the flag.

## [0.61.0] — 2026-06-17

Maintainer-facing internals cleanup — no behavior change.

### Changed

- **Split the 1,326-line `jobs_exec.rs` into focused modules.** The web background-job executor is
  now `jobs_exec/{mod, watchdog, deep}.rs`: the 680-line `run_deep_phase` (the parse → chunk → embed
  loop with caption / transcribe / OCR / video sub-passes) and the memory watchdog each live in their
  own file, leaving the orchestration + scan/summarize phases in `mod.rs`. A pure module move (no logic
  change) — verified by the unchanged test suite, clippy, and an end-to-end `index` run exercising the
  full scan → deep → summarize pipeline through the split code.

## [0.60.0] — 2026-06-17

Sliced exports, everywhere: the `--changed-since` / `--category` filters now work on every export surface, not just `indexa export`.

### Added

- **`indexa pack export --changed-since <window>` / `--category <name>`** — the relational slice
  from v0.58 now applies to pack exports too (e.g. export just the code in a pack, or just what
  changed this week).
- **Web export slicing** — `GET /api/export` and `GET /api/packs/:name/export` accept
  `changed_since` and `category` query params, and the web workspace's Export menu gains two
  optional "changed since" / "category" fields so it's usable from the UI, not just the API.
- The shared filter is now `indexa_query::build_export_filter` (lifted out of the CLI), so all four
  export surfaces — CLI `export`, CLI `pack export`, web `/api/export`, web pack export — slice
  identically and stay in sync.

### Changed

- A web export whose slice matched nothing returns `404` (or `422` for packs) with a clear
  "widen the slice" message instead of an empty `200` — the same honesty guard the CLI already has.

## [0.59.0] — 2026-06-17

See the savings: every answer now shows how much smaller the served context was than the source it drew on.

### Added

- **Per-answer impact readout.** Each `ask` now reports the concrete "retrieve the slice" win for
  *that* answer — e.g. `served 4.2 KB vs 1.8 MB of source — 99% less to your AI tool`. It appears under
  the answer in the web workspace, as a line in the CLI (`indexa ask`), and as an `impact` object
  (`served_bytes` / `counterfactual_bytes` / `saved_percent`) in `indexa ask --json` and the
  `POST /api/ask` response. The README's token-savings pitch is now verifiable at the point of use,
  not just in aggregate `status`. The figure is honest: it compares against the **cited** files (not
  the whole repo), is shown only when there was a real win, and is capped at 99% (a real answer always
  serves *something*). The byte counts come from the existing usage telemetry — no extra work per query.

### Changed

- **`indexa ask` usage telemetry now counts the citation bytes it serves, not just the answer text.**
  The CLI's recorded "served" figure (the basis for the cumulative savings in `indexa status`) previously
  counted only the answer text; it now matches the web surface (answer + delivered citations). This
  slightly *lowers* reported aggregate savings going forward — the old number modestly overstated them.

## [0.58.0] — 2026-06-17

Sliced exports: hand your AI tool exactly the part of the repo that matters — and an export now fails loudly instead of writing an empty file.

### Added

- **`indexa export --changed-since <window>`** — export only files modified within a window
  (e.g. `7d`, `12h`, `90m`, `3600s`), plus the directories on the path to them. A relational
  slice that reuses the index's recorded mtimes — no re-scan. Great for "give my AI tool just
  what changed this week."
- **`indexa export --category <name>`** — export only files in a classification category
  (e.g. `code`, `document`, `media`, `work`). Combine with `--changed-since` to intersect
  ("code I touched this sprint"). Reuses the classification table.

### Fixed

- **`export` and `pack export` no longer silently succeed when they produced nothing.** Previously,
  exporting with no index, no summaries, or a slice that matched no files printed a notice **to
  stdout** and exited `0` — so `indexa export > ctx.xml` wrote the notice *into* the file and a CI
  step saw success. Both now error to stderr with a non-zero exit, so a piped/automated consumer can
  trust that a successful export is a valid artifact.

## [0.57.0] — 2026-06-17

A maintainer on-ramp: the codebase is easier to read and contribute to, with **no change to behaviour** (every existing test still passes, unchanged).

### Changed

- **The 2,377-line Q&A pipeline is split into focused modules.** `crates/query/src/qa.rs` is now
  `qa/{confidence, mmr, retrieve, explain, synthesize, agentic}.rs` (+ `mod.rs` for the shared types
  and the public re-exports). The `qa::` public API is byte-identical — a pure file move with
  `pub(crate)` visibility, proven by the unchanged test suite.
- **The 3,535-line store test file is split by concern.** `crates/core/src/store/tests.rs` is now
  `store/tests/` — one file per concern (basics, queue, packs, weights, insights, graph, scoped
  resolution, entry cleanup, decisions, usage, incremental) with shared fixtures in `tests/mod.rs`.
  All 132 tests preserved.
- **The web `ask` retrieval-config mapping is extracted to pure functions** (`qa_config_from` /
  `agentic_from`), so it can be unit-tested without constructing a full server `AppState`.

### Added

- **Contributor docs.** A new [how-to: add an MCP tool](docs/how-to/add-an-mcp-tool.md) (tools live in
  router modules, **not** `lib.rs`), a [web-UI assets README](crates/web/assets/ui/README.md)
  documenting the `include_str!` concat-order convention, `//!` module headers on `indexa-core` and
  `indexa-cli`, and a CONTRIBUTING section on adding an MCP tool plus the CI / desktop-build gate.
- **More unit tests** — `apply_mmr` no-op edge cases (fewer than two candidates; no embeddings) and the
  web `ask` config-mapping (scope normalization, agentic flag precedence).

### Fixed

- **Corrected the contributor map in `docs/architecture.md`.** The "Add an MCP tool" row pointed at
  `crates/mcp/src/lib.rs` and `#[tool]`; tools are actually defined across eight router modules and
  composed with `+` in `tool_router()`. The "store table" row now points at the new `store/tests/`
  layout.

## [0.56.0] — 2026-06-17

A flawless first run: no silent model wall, no dead buttons.

### Added

- **Indexa offers to pull the local models for you.** The first time you `index` (or run any command
  that needs Ollama), if a required model isn't pulled, Indexa now asks — "Download them now? [Y/n]" — and
  pulls each with a **live progress bar**, instead of failing with a manual `ollama pull` instruction and a
  silent ~11 GB wait. New `indexa_llm::ollama_pull` streams Ollama's `/api/pull` progress (no new
  dependency). In a non-interactive shell (CI/pipes) it still prints the manual instruction and exits.

### Changed

- **`indexa deep` shows rate + ETA.** The per-file progress line now reads `[120/4000] 42/s · ETA ~1m ·
  <file>`, so a long indexing pass visibly progresses instead of looking stuck.
- **README first-run is honest about setup.** The install section now leads with the auto-pull + `indexa
  doctor`, rather than presenting the 11 GB download as a manual prerequisite.

### Fixed

- **The "Add a folder" button worked.** `openAddRoot()` was referenced by the empty-state CTA and the
  sidebar "+" but never defined — clicking it threw. It now prompts for a folder path and kicks off an
  index job.

## [0.55.0] — 2026-06-17

The knowledge graph becomes the flagship view, and the README stops lying.

### Changed

- **The interactive code graph is now the default Map view, and it animates.** Opening **Map** lands on the
  force-directed knowledge graph (was a third sub-tab behind the treemap), and the layout now *blooms* into
  shape via an animated cooling loop instead of snapping into place. Added **wheel-zoom** (around the cursor)
  and **drag-to-pan**. Nodes are still sized by centrality; edges still show the honest resolution tier
  (solid = confident, dashed = approximate bare-name match). Respects `prefers-reduced-motion` (settles
  instantly). All vanilla SVG — no new dependencies.
- **The README's "See it work" output is now real.** The previous console block showed fabricated numbers and
  an invented answer; it's replaced with Indexa's actual 3-phase indexing output and a real `ask` answer
  (Indexa indexing its own repo, so the numbers are reproducible). For a project whose pitch is "every claim
  is true," the front page now holds to it.

## [0.54.0] — 2026-06-17

Deeper code-graph reachability and richer slide parsing.

### Added

- **Transitive blast radius.** "What breaks if I change X?" can now follow the call chain more than one
  hop. The `blast_radius` MCP tool takes a `depth` (1 = direct callers, 2 = +one transitive hop = the old
  default, up to 5 for reach through chains), and `indexa graph --blast <symbol> --depth N` exposes the
  same from the CLI. The walk is cycle-safe (each file is visited once) and bounded.
- **Chart & SmartArt text from slides.** `.pptx`/`.ppsx` indexing now extracts text from embedded charts
  (`ppt/charts/chartN.xml`) and SmartArt diagrams (`ppt/diagrams/dataN.xml`) as deck-level chunks, so the
  numbers and labels living in a chart or org-diagram are searchable — closing a documented parsing gap.
  (Styling/layout diagram parts are excluded; embedded OLE objects still aren't extracted.)

## [0.53.0] — 2026-06-17

The v0.52 cumulative changelog now shows up everywhere, plus a few honest edges.

### Added

- **"What's new" in the web Software Update panel.** When an update is available, Settings → Software
  Update lists every version's notes between your installed version and the latest — the same span the
  desktop modal shows — fetched only when an update exists, and silently omitted if it can't be fetched.
- **"What's new" in `indexa update`.** The CLI now prints the cumulative changelog before the confirm
  prompt (and on `indexa update --check`), so you see what you're getting before installing.

### Changed

- **Archives tell you when a listing is capped.** A `.zip`/`.tar` with more than 5,000 entries now appends
  "(listing truncated — showing first 5000)" instead of silently showing a partial count as if it were whole.
- **Model pulls can't hang forever.** The web model-download proxy now aborts with a clear error if Ollama
  goes silent for 120s mid-stream (a live, slow download is unaffected — there's no overall timeout).
- **MMR diversity is clamped to a sane range.** A hand-edited out-of-range `[retrieval] mmr_lambda` can no
  longer invert relevance; it's clamped to `[0, 1]`.
- **Docs corrected:** the repo is public (not private); the contributing notes reflect that branch
  protection — not Actions billing — is what requires the PR + CI workflow.

## [0.52.0] — 2026-06-17

The update window now respects your history.

### Changed

- **The update window shows every version you're getting, not just the newest.** When the desktop app
  updates across several releases (e.g. 0.49 → 0.52), the changelog modal now lists each in-between version's
  notes — assembled from the release-tagged `CHANGELOG.md`, newest first — instead of only the latest section.
  `latest.json` can't do this on its own: it's baked at release time and doesn't know which version you're
  coming from, so only the app assembles the span. It fails open — if the changelog can't be fetched, it
  falls back to the single newest section, so an update is never blocked. (The assembling code runs in the
  app *doing* the updating, so this takes effect for updates **from 0.52 onward** — the jump to 0.52 itself
  still shows only 0.52's notes.) No new dependencies; reuses the existing rustls HTTP client.

## [0.51.0] — 2026-06-16

UX polish from the experience audit. (Several audited items — job-warning badges, model-role chips, the
graph "approximate edge" caveat, the chunk tooltip — were already in place; these close the genuine gaps.)

### Changed

- **Scoped Ask shows the way out.** When a question is scoped to a file/folder, the answer now carries an
  inline note ("Answered within X only — clear the ✕ chip to search everywhere"), so a thin or empty
  scoped reply isn't mistaken for "Indexa doesn't know it."
- **CLI errors point somewhere.** A failed command now prints a Troubleshooting line — run `indexa doctor`
  (checks Ollama liveness, models, config) or read the log at `<data-dir>/logs/indexa.log` — then exits
  non-zero, instead of a bare error.

## [0.50.0] — 2026-06-16

"Format Wave 3": Apple iWork, binary symbols, and Outlook `.msg` — three more types Indexa understands.

### Added

- **Apple iWork** (`.pages` / `.numbers` / `.key`) — extracts text from the embedded preview PDF (rendered
  snapshot, via the existing PDF path; **zero new dependency**). A file without a preview yields a quiet stub.
- **Binary symbols** (`.so` / `.dylib` / `.exe` / `.o` / `.wasm` / `.jar`) — lists the symbols/exports a
  compiled artifact declares (ELF/Mach-O/PE via `object`, wasm via `wasmparser`, `.jar` class names), so
  "what defines X" can reach a binary. Names only — no disassembly; a stripped binary is a quiet stub.
- **Outlook `.msg`** — now extracted (subject + body from the OLE MAPI streams via `cfb`) instead of a
  stub. Legacy `.ppt`/`.doc` (PowerPoint/Word binary) remain honest stubs.

### Notes

- New deps `object`, `wasmparser`, `cfb` are Apache-2.0/MIT and pure-Rust — verified openssl-free on host
  **and** `aarch64-unknown-linux-gnu`, and `cargo deny` (licenses + advisories) clean.
- `indexa formats` now lists 84 formats; `docs/methodology.md` updated. Recursive archive extraction (vs.
  the current listing) remains a deliberate non-goal for now.

## [0.49.0] — 2026-06-16

"Know what's supported, see what's using your RAM": a queryable formats list, an honest memory readout,
and clearer empty states.

### Added

- **`indexa formats`** (+ the MCP `list_supported_formats` tool — now 46) lists every file format Indexa
  understands with its support level (full = text extracted · metadata = listing/EXIF only · stub =
  recognised but not extracted · textfallback = sniffed as text) and MIME type; `--json` and `--level`
  filters. Backs the "understands every file" claim with a queryable, drift-tested list — parsers declare
  their formats via the new `Parser::declared_formats`.
- **"What's using my RAM" readout** — the engine bar gains a read-only popover (and `GET /api/engine/processes`)
  listing the top memory-consuming processes so you can decide what to quit. Indexa **does not** purge or
  kill other apps' memory: that's invasive and counterproductive (macOS hands back free cache on demand).
  This is the honest, useful answer, alongside the existing Free Models button and conservative profile.

### Changed

- Clearer empty states: the Map and folder tree now say "no summaries built yet — index for search"
  (distinct from a bare scan), and the ⚡ row-action tooltip explains the deep/embed phase vs. scanning.

## [0.48.0] — 2026-06-16

"Understand even more files" (format Wave 2): email, archives, and opt-in scanned-PDF OCR.

### Added

- **Email `.eml`** — parsed with `mail-parser` (Apache-2.0/MIT, pure Rust): subject, from/to, date,
  body (plain text, or the HTML part converted), and attachment *names*. `.msg` (Outlook OLE) gets a
  quiet stub rather than a hard error.
- **Archives `.zip` / `.tar` / `.tar.gz` / `.tgz`** — listed by entry name + size, so an archive is
  searchable by the files it contains. Shallow (entry contents are not extracted — avoids zip bombs);
  registered after the Office/EPUB parsers so it never claims their zip containers.
- **Scanned-PDF OCR (opt-in).** Set `[parsers.pdf] backend = "ocr"` to OCR PDFs with no text layer:
  pages are rasterised with `pdftoppm` (poppler) and recognised with `tesseract` (`ocr_binary` /
  `ocr_lang` configurable). Both are external tools; OCR fails open to the text layer when they're
  unavailable, and runs in the indexing pipeline (CLI `deep`/`index` and the web build).

### Fixed

- **Dead PDF `backend` config.** `[parsers.pdf] backend` defaulted to `"pdfium"` but nothing ever read
  it (no pdfium dependency existed). The default is now the honest `"text"`, with `"ocr"` wired.

### Notes

New dependencies `tar`, `flate2`, `mail-parser` are all Apache-2.0/MIT and pure-Rust — verified
openssl-free on host and `aarch64-unknown-linux-gnu`, and `cargo deny` clean.

## [0.47.0] — 2026-06-16

"Read the web too": HTML files are now parsed structurally instead of as flat text.

### Added

- **HTML parser** (`.html`/`.htm`/`.xhtml`). `<script>`/`<style>` are stripped, the page is converted
  to Markdown (htmd — Apache-2.0, already used by the remote-source fetch path), then chunked through
  the shared heading-aware Markdown sectioner, so an `<h1>`/`<h2>` structure becomes the same breadcrumb
  headings a `.md` file gets. Previously `.html` fell through to the fixed-window text parser.
  Dynamic/JS-rendered content is not executed; tables flatten to Markdown.

### Changed

- Markdown sectioning is now a shared `chunk_markdown` helper used by both the Markdown and HTML
  parsers — no duplicated logic.

## [0.46.0] — 2026-06-16

"Understands more files": new parsers for Jupyter notebooks, SVG, and clean RTF, plus Markdown
frontmatter extraction — and the built-in parser list now lives in exactly one place.

### Added

- **Jupyter `.ipynb`** — one chunk per cell (code cells language-tagged from the kernel; outputs
  skipped), so notebooks are indexed as their source instead of noisy JSON.
- **SVG `.svg`** — extracts `<text>` / `<title>` / `<desc>` content; path geometry and CSS are
  ignored (no OCR of outlined glyphs). Diagrams become searchable by their labels.
- **Markdown frontmatter** — a leading `---` YAML block (`title`/`tags`/`date`/`description`/`author`)
  is lifted into a searchable chunk instead of being treated as body text.

### Changed

- **RTF is parsed, not dumped.** `.rtf` now goes through a control-word stripper that drops markup
  and skips font/colour/style tables and `\*` ignorable destinations, leaving the prose — previously
  the raw control words leaked into the index.
- **Single parser registry.** The built-in parser list was duplicated between `Registry::new` and the
  free `parse` function; the free function now delegates to the registry, so a new parser is added in
  one place.

### Notes

`docs/methodology.md` now documents the full parser table plus an honest "known parsing gaps" list
(structured HTML, OCR, email, archives, Apple iWork, binary symbols) — the next format waves.

## [0.45.0] — 2026-06-16

"Summaries that name the API": file summaries are now built from a representative sample across the
file's chunks (not chunk 0 alone), and code files get their public symbols injected so the summary —
and retrieval — can name the functions and types a file actually exports.

### Changed

- **Richer summary input.** `summarize` composes each file summary from a document-level sample across
  the file's chunks (via the shared contextual `build_doc_context`, bounded to 4 000 chars) instead of
  the first chunk alone, so multi-section files are represented rather than just their opening.
- **API surface in code summaries.** For code files, the symbols the file *defines* (from the stored
  `defines` code-graph edges — no re-parse) are prepended to the summary input, with idiomatic noise
  (`new` / `default` / `with_*` / `get_*` / …) filtered via the shared `is_idiom_symbol` denylist. File
  summaries now name the real functions/types a file exports, so "which file defines X" and "what does
  this file export" retrieve and answer better. Both enrichments reuse already-indexed data (no extra
  LLM call) and fail open to the plain sample. Existing summaries regenerate only when their file
  content changes or you re-run `summarize`.

## [0.44.0] — 2026-06-16

"What is this project?": the no-argument project overview and tree root now work, retrieval defaults
are tuned to surface specifics instead of "the context doesn't say," and the README leads with how
Indexa rolls a summary of every file up into folder and whole-project context.

### Fixed

- **`project_overview` (no scope) and the tree root listing returned nothing** even though the
  roll-up existed. `Store::root_paths()` returned the indexed root's *un-indexed filesystem parent*
  instead of the indexed root directory itself, so the overview resolved to a summary-less path and
  walked away from the data; `tree_level("")` matched on an empty `parent_path` that no row carries.
  Both now resolve the indexed roots correctly — a pure read-path SQL fix, no re-index needed — so
  `browse_tree`, the web tree's first load, and `project_overview` answer "what is this project?"
  out of the box. As a side effect, `read_file` / file-preview path-confinement is tightened: files
  are confined to the actually-indexed project, not its broader filesystem parent.

### Changed

- **Sharper answers by default.** Retrieval now reranks by default (`[retrieval] rerank = true`,
  `"llm"` backend — reuses the already-loaded generation model, no download, fails open), retrieves
  a wider candidate pool (`top_k` 8 → 12), and packs more context into synthesis (`context_budget`
  4000 → 8000 chars), so answers cite concrete specifics. Set `rerank = false` to opt out. Code-intent
  questions ("which function implements…") bias MMR toward relevance so the implementing file's own
  chunks survive the diversity pass.

### Documentation

- **README leads with the roll-up.** A new *How the context builds* section + a committed diagram
  (`docs/assets/rollup-tree.svg`) show per-file → per-folder → whole-project synthesis and the
  L0 / L1 / L2 tiers. Added an `indexa summarize` data-flow to `docs/architecture.md`, a hierarchical
  summarization & roll-up section to `docs/methodology.md`, and framed the persistent roll-up as the
  competitive moat in `docs/COMPETITIVE.md`.

## [0.43.1] — 2026-06-16

"Sharper retrieval": a local DeBERTa-v2 cross-encoder reranker joins the LLM reranker, and agents
can now explicitly opt into reranking per query via the MCP `ask` tool. (The v0.43.0 tag failed to
build for Linux ARM64 and never produced a release; v0.43.1 is the first published v0.43 build.)

### Added

- **Candle cross-encoder reranker.** Set `[retrieval] rerank_backend = "cross-encoder"` to use
  `mixedbread-ai/mxbai-rerank-xsmall-v1` (Apache-2.0, ~85 MB, DeBERTa-v2 xsmall) for pointwise
  (query, doc) scoring instead of the LLM listwise reranker. Pure-Rust candle inference — no
  onnxruntime, no native dylib, safe for macOS notarized builds. Model downloaded from HuggingFace
  on first use and cached in `~/.cache/huggingface/hub/`. The model initialises once per process
  (mmap-backed; OS page-cache keeps it warm). Falls back to the LLM reranker on any load error.
- **`rerank` + `rerank_backend` on MCP `ask` tool.** Agents can now explicitly enable reranking
  (`rerank: true`) and choose the backend (`rerank_backend: "llm" | "cross-encoder"`) per call,
  rather than inheriting the server default. Both params default to the server config when omitted.
- **`rerank_backend` config key.** `[retrieval] rerank_backend = "llm"` (default, listwise LLM)
  or `"cross-encoder"` (candle DeBERTa-v2). Works with all surfaces: CLI `ask`, web, MCP.

### Fixed

- **Linux ARM64 release build.** `hf-hub` now uses `ureq` + rustls (`features = ["ureq"]` on
  hf-hub 0.5, ureq 3's default rustls + ring backend) instead of the v0.43.0 native-tls path, which
  pulled `openssl-sys` and failed to cross-compile to `aarch64-unknown-linux-gnu` — the failure that
  blocked the v0.43.0 release. The tree is now openssl-free, matching the existing reqwest/rustls
  invariant. Verified with a real ARM64-Linux cross-build.

## [0.42.0] — 2026-06-16

"Fast, legible & visible": re-indexing is dramatically faster (skips unchanged chunks), retrieval
is more diverse (MMR), three new MCP tools surface what the engine already knew, and the product
finally shows you what it can do.

### Added

- **Embedding cache (content-hash per-chunk).** Re-indexing now skips unchanged chunks: each chunk
  gets a SHA-256 of its raw text; on subsequent `deep` / `index` runs, only chunks whose text
  changed are sent to the embedder. The first re-index after any edit is now proportional to
  *what changed*, not to the size of the whole file. Existing databases upgrade transparently
  (new `content_hash` column, nullable for legacy rows).
- **MMR diversity in retrieval.** Context packing now applies Maximal Marginal Relevance to
  re-score candidates before budget fill, penalising near-duplicate chunks (slide footers, licence
  blocks, repeated boilerplate). Tune with `[retrieval] mmr_lambda` (0 = max diversity, 1 = off;
  default 0.5). Never drops a hit — only reorders.
- **Three new MCP tools (now 45 total):**
  - `project_overview` — synthesise a plain-language summary of the whole indexed project (or a
    scoped subtree) in one call; much faster than `ask` for "what is this project about?".
  - `explain_retrieval` — return the full retrieval trace for any question (sparse/dense/fused
    stages, top-k scores); use it to understand or debug why `ask` returned specific sources.
  - `inspect` — return all indexed facts about a path (kind, size, chunks, language, summary,
    model, category, weight, code-graph edges); the same data as the web "Indexed facts" panel.
- **`indexa describe` whole-project mode.** Running `indexa describe` with no path now prints the
  project overview instead of requiring a specific file path.
- **Auto-preflight for `indexa index` / `deep`.** When Ollama is the provider and it isn't running
  or a required model isn't pulled, the command now prints an actionable hint ("Start Ollama" /
  "run: ollama pull X") before any work begins, instead of failing mid-pipeline with a raw error.
- **Plain-language UX polish.** "Why these sources?" trace gets a "How Indexa found these files"
  caption; graph centrality tooltip glosses "how many files depend on it"; the staleness health
  banner now has a one-click "Re-index now" button; the Ask welcome and onboarding copy surfaces
  project-level questions and document/presentation indexing.
- **Correct first-step hints.** CLI "no index found" messages now recommend `indexa index <path>`
  (the one-shot pipeline) instead of the power-user `scan`/`deep` stages.
- **Shared text utility.** `indexa_core::text::truncate_chars` / `snippet` unify three previously
  inconsistent char-boundary truncation idioms across the codebase.

### Changed

- **Simplified web contextual-retrieval path.** The web deep-scan blurb loop now calls the same
  `contextual_embed_texts` helper as the CLI (killed the last prompt-drift risk between paths).
- **Deduplicated ollama.rs prompt builders.** `describe` / `describe_stream` and `summarize_dir` /
  `summarize_dir_stream` now share a single private prompt-building function each (previously the
  prompt strings were duplicated between the stream and buffered variants).
- **Removed dead `micro_benchmark` config field.** The `[resource] micro_benchmark` field was
  declared and parsed but never read; its promised behaviour was never implemented. Removed.
- **Deduplicated JS escape helpers.** Four `escapeHtml`-equivalent functions (`escW`, `escI`,
  `escG`, `esc`) scattered across the web UI's JS files were consolidated into the single canonical
  `escapeHtml` defined in `08-util-palette-init.js`.

## [0.41.0] — 2026-06-16

"Understand the whole project": Indexa now makes sense of a real work directory — presentations,
documents, spreadsheets, and code — and can answer "what is this project about?" from the project's
own structure rather than from scattered file excerpts.

### Added

- **Presentation parsing (`.pptx`/`.ppsx`).** PowerPoint files are fully indexed for the first
  time — slide text and speaker notes extracted per slide, sorted numerically (slide 10 after slide
  9), with one searchable chunk per slide. `.ppt` (legacy binary) now stores a quiet fallback stub
  instead of counting as a parse error.
- **Richer Word document parsing.** `.docx` extraction now includes headers, footers, footnotes,
  and endnotes in addition to the main body — so a document's full text is indexed, not just
  its paragraphs.
- **Whole-project synthesis.** Broad questions — "what is this project about?", "main themes",
  "high-level overview", "summarize the project" — now draw on Indexa's directory roll-up
  summaries (always generated, never surfaced in answers before). The answer opens with a
  PROJECT OVERVIEW block: the root directory's summary plus top child-directory one-liners.
  Specific questions keep the existing chunk-citation behaviour unchanged.
- **Contextual Retrieval (`--contextual`).** Anthropic's Contextual Retrieval technique is now
  available on the CLI path. Pass `--contextual` to `indexa deep` or `indexa index` to generate
  a 1–2 sentence situating blurb per chunk before embedding — reduces retrieval failures by ~35%
  at the cost of one extra LLM call per chunk (default: off; or set
  `[describer] contextual_retrieval = true`). The web path already supported this; CLI and web
  paths now share a single prompt (no drift).

### Changed

- **549 tests** (up from 520). New tests cover PPTX slide ordering, speaker notes, fallback on
  corrupt zips, broad-intent detection, project-overview budget, contextual blurb generation,
  cancellation, and all CLI flag parsing additions.

## [0.40.0] — 2026-06-15

"Readable & quiet": the in-app update window now renders the changelog as formatted text, and
the review inbox stops asking about files that merely share a topic.

### Fixed

- **The "What's new" window no longer shows raw markdown.** Headings, **bold** text, bullet
  lists, and `inline code` now render properly instead of showing `**`, `###`, and `-` as
  literal characters. The formatter handles the hard-wrapped CHANGELOG style so bullet
  continuations flow as a single item, not a broken list.
- **The Review inbox no longer flags unrelated files as duplicates.** Near-duplicate detection
  (similarity ≥ 95%) now requires all cluster members to share the same filename — so two
  different files that happen to cover similar topics (e.g. `summarize.rs` and `jobs_exec.rs`)
  no longer produce a "which is canonical?" question that has no useful answer. Exact-content
  duplicates (byte-identical regardless of name) still always ask. Existing false-positive
  questions are cleared automatically on the next `indexa prune`.

## [0.39.0] — 2026-06-15

"Trustworthy & current": a quieter review inbox, code answers for code questions, and an
end to silently-stale context.

### Fixed / Changed

- **The Review inbox no longer floods you with unanswerable questions.** Duplicate detection now
  skips near-identical **assets** (icon sets, screenshots, fonts) and generated/vendored trees — it
  only asks about redundant **source/text** you could actually consolidate. The "which definition is
  authoritative?" (symbol-ambiguity) detector is now **off by default**, and when enabled it skips
  universal idioms (`new`, `default`, `parse`, `build`, …) and symbols defined in many files — cases
  that have no answer on an idiomatic codebase. Existing low-value questions are cleared automatically
  on the next index or `indexa prune`.
- **Code questions get code answers.** Retrieval now lifts the implementing source file above prose
  docs when you ask an implementation question ("which function does X", or you name a `snake_case`
  symbol) — so "how does X work?" stops returning only the README/CHANGELOG.
- **Your CLI tracks the app.** When the desktop app self-updates it now refreshes the matching `indexa`
  CLI in place — fixing the silent version skew where the app moved ahead while your terminal `indexa`
  (and the MCP server that runs it) stayed versions behind and served stale answers.

### Added

- **Staleness and version are now visible.** `GET /api/health` plus a web banner warn when the index is
  stale (newest content older than a week); the MCP `get_stats` tool now reports the running server
  version and index age, so an AI agent can tell when it's talking to a stale binary or index instead of
  trusting it blindly.

### Notes

- Background auto-watch (live re-indexing of changed files) and a `doctor` version-skew section are the
  next step; for now the staleness banner + `get_stats` make a stale index visible so you can re-index
  or start Watch.

## [0.38.0] — 2026-06-14

"Safe": the memory watchdog now sees the vision models too.

### Added / Fixed

- **Image/video captioning is counted by the memory watchdog.** Captioning runs a vision model
  **alongside** the summary models, so its memory adds up. The watchdog now knows the footprint of
  common vision models (`llama3.2-vision`, `moondream`) and, when you enable captioning, checks the
  **combined** peak against your budget. If it won't fit, you get an honest warning with a lighter
  model suggestion (e.g. `moondream`) — captioning is still saved (you own your machine), just no
  longer a silent freeze risk. An unknown caption model is flagged as un-sizable rather than counted
  as zero. (Audio transcription is excluded — it runs an external `whisper-cli` process, not an Ollama
  model.) The old "Not yet counted by the memory watchdog" notes are gone.

## [0.37.0] — 2026-06-14

"Durable": your view survives a reload, a bookmark, and a relaunch.

### Added

- **Deep-linkable URL state (web).** The active tab, the selected file, and your last Ask question are
  now encoded in the page URL (`#tab=…&path=…&q=…`), so a view is **bookmarkable and shareable** and is
  **restored on reload** — instead of always booting to a blank Context view. Restoring a question types
  it back into the box but never auto-runs it; restoring a path fetches exactly one summary (no request
  storms). Back/Forward navigation works. Local file paths in the URL only resolve on your own machine.
- **Desktop window state persists across launches.** The app now remembers its **size and position**
  when you quit and relaunch (via the first-party `tauri-plugin-window-state`); the configured
  width/height become first-launch defaults, and the minimum-size clamp keeps a restored window
  on-screen.

## [0.36.0] — 2026-06-14

"See the graph": the call graph becomes a navigable knowledge map you can explore.

### Added

- **Navigable knowledge graph in the Map tab.** The Graph view is now interactive:
  - **Click any file (or press Enter)** to focus it — its callers and callees light up,
    everything else dims, and a breadcrumb appears.
  - **Expand neighbors** re-fetches just that file's connections (1 hop, then 2), so a
    hub's real neighbors are never lost to display limits — and **Show all in scope**
    returns to the full view.
  - **Nodes are sized by how central a file is** (weighted PageRank) and **edges are
    styled by relation** — solid for clear references, dotted for approximate name-only
    matches.
  - A **legend** and a plain-language **"What is this?"** explainer make it readable for a
    general audience, and the approximate (name-only) caveat is surfaced honestly whenever
    such links are shown.
- `GET /api/graph` gained optional read-only `focus`/`depth` query parameters that return a
  file's N-hop neighborhood (server-side filtering of the already-scoped graph; no schema
  change, no new dependency).

## [0.35.0] — 2026-06-14

"Legible retrieval": see *why* an answer cited what it did, and what's indexed for any path.

### Added

- **"Why these sources?" on every answer.** Each Ask answer now has an expander that shows the
  retrieval trace — the sparse (keyword), dense (semantic), and fused (RRF) stages with each hit's
  rank and score — so you can see how a source surfaced, not just that it did. (The web equivalent
  of `indexa ask --explain`; computed on demand via `POST /api/ask/explain`.)
- **"Indexed facts" under every summary.** A collapsible panel showing exactly what Indexa stored
  for the selected path: kind, size, last-modified, chunk count + language, whether a summary exists
  (and which model), classification, importance weight, and code-graph edge counts — with a note
  that it's a derived cache, re-derivable by re-indexing, and your files are never modified. (The web
  equivalent of `indexa inspect`; `GET /api/inspect`.)

## [0.34.0] — 2026-06-14

A real update window, and a file preview beside every summary.

### Added

- **An in-app update window.** "Check for Updates…" no longer pops a cramped system dialog. When an
  update is found, Indexa shows a proper window — a white card with the **full, scrollable
  changelog** and **Install & Relaunch** / **Later** — then the live progress bar, then restart.
  The whole flow is in-app now, like a normal app's updater.
- **File preview beside the summary.** Selecting a file in the Context tab now shows its **actual
  contents** in a pane next to the summary, with lightweight syntax highlighting (a built-in
  tokenizer — no third-party library), line numbers, and a binary/large-file notice. Toggle it with
  the **Preview** button (your choice is remembered). Reads are confined to your indexed folders and
  capped at 40 KB.

### Changed

- The desktop updater is fully in-app: the macOS confirmation dialog was removed in favor of the new
  window, and a double-trigger (tray + menu) can no longer start two downloads.

## [0.33.0] — 2026-06-14

"Trust & position": see exactly what's indexed, and a clearer story about why retrieval beats packing.

### Added

- **`indexa inspect <path>`** — a plain-text "what's indexed here" view for any path: the scan entry
  (kind/size/modified), the indexed chunks (count + first symbols/headings), whether a summary
  exists, the classification, the resolved importance weight, and the code-graph relationships
  (imports/defines/calls). It ends by noting that **the index is a derived cache over your real
  files — every field is re-derivable by re-indexing, and your source files are never modified**, so
  the index is legible rather than a black box.

### Changed

- **Sharper positioning** (`docs/COMPETITIVE.md`): the wedge vs whole-repo packers (Repomix / gitingest
  / code2prompt) is now stated with the concrete token reality — a real repo packs to tens of millions
  of tokens — and points at v0.31's `--signatures` / `--token-budget` / on-export secret-scan as the
  "if you must pack, pack smart" answer. **Retrieve the slice; don't pack the repo.**

## [0.32.0] — 2026-06-14

"Reach": pull a few remote sources into a Context Pack — opt-in and local-first.

### Added

- **`indexa pack add-url <pack> <url>`** — fetch a **GitHub issue/PR** (via the public API, already
  Markdown — title, state, body, and comments) or **any web page** (HTML → Markdown), and cache it
  as a local file the pack can index, search, and export like any other. Re-fetching the same URL
  overwrites in place. Optional `--label` names the cached file.
- **Opt-in + local-first.** Fetching reaches the network, so it's **off by default** — enable it
  with `[sources] enabled = true` in config.toml, or `INDEXA_REMOTE_FETCH_ALLOW=1` for one run.
  Honors `GITHUB_TOKEN` for higher GitHub rate limits. The fetched content lands as a plain local
  file (under the data dir); nothing is sent anywhere. `<script>`/`<style>` blocks are stripped
  before conversion so a page's CSS/JS doesn't pollute the context.

### Notes

Scope is deliberately narrow (GitHub + generic web). arXiv/YouTube and other site-specific scrapers
belong in optional plugins, not core — they break often. Saved pack "recipes" are deferred to a
later release.

## [0.31.0] — 2026-06-14

"Exports that fit": smarter, safer context exports — and answers can prefer fresh files.

### Added

- **Code-skeleton exports.** `indexa export --signatures` (and `pack export --signatures`, MCP
  `export_pack signatures=true`, web `?signatures=1`) emit each symbol's signature + leading
  docstring with bodies elided — hand an AI tool your code's *structure* at a fraction of the
  tokens, instead of either full files or prose summaries. Heuristic and line-based; works after
  `deep`, even without summaries.
- **Token-budget guard.** `export --token-budget N` warns when an export exceeds ~N tokens
  (≈4 chars/token); add `--strict-budget` to fail with a non-zero exit (handy in CI/agent loops).
- **Secret scanning on export.** Every export (CLI, MCP, web) is now scanned for obvious
  credentials — AWS keys, GitHub/Slack/Google/OpenAI tokens, private-key blocks, `key = "…"`
  assignments — and they're redacted before the content leaves your machine. Opt out per-run with
  `--no-redact`. A safety net, not a guarantee.
- **Clipboard + hygiene flags.** `export --clipboard` copies the result straight to the OS
  clipboard (via the platform's native tool — no extra dependency); `--strip-comments` drops leading
  doc-comments from a `--signatures` export.
- **Recency-aware retrieval (opt-in).** Set `[retrieval] recency_boost = true` to bias answers
  toward recently-modified files — the positive twin of the archive down-weighting from v0.29. Uses
  file modification time (not git); window configurable via `recency_days` (default 90).

### Why

Tools that pack a whole repo into one file routinely blow past context limits (tens of millions of
tokens). Indexa already serves a *retrieved slice*; these changes make the exported slices smaller
(signatures), safer (secret-scan), and easier to keep within a model's window (token budget).

## [0.30.0] — 2026-06-14

A small, focused release: you can now watch updates download.

### Added

- **A live download progress bar for updates.** "Check for Updates…" still shows the new version and
  what changed, but once you confirm, an in-app bar now fills as the new version downloads, switches
  to "Installing…", and then the app restarts — so you can see it working instead of waiting on a
  silent dialog. The **"Install command-line tool"** action shows the same live bar.

### Changed

- **Clearer command-line-tool install message.** A desktop app can't see your terminal's `PATH`, so
  instead of wrongly claiming a folder "isn't on your PATH," it now says to add the folder *if*
  `indexa` isn't found in a new terminal.

## [0.29.0] — 2026-06-14

"Trustworthy & legible": answers you can trust, a Map that keeps up, a sidebar you can read, and
updates that work like a normal app.

### Fixed

- **Answers stop citing archived, out-of-date docs.** Asking about the repo could surface content
  from an `archive/` folder and confidently state a version that hasn't shipped in years. Retrieval
  now **automatically down-weights** files under historical path segments (`archive`, `archived`,
  `historical`, `deprecated`, `old`) so current sources win — while the archived files stay
  **findable** if you explicitly scope a question into them. Nothing is deleted from your index.
- **Answers no longer drift into invented follow-up questions.** The model sometimes continued the
  context as a fake transcript ("QUESTION: … ANSWER: …") instead of answering what you asked. The
  prompt now instructs it to answer only your question and prefer current over archived sources, and
  any invented `QUESTION:`/`ANSWER:` continuation is trimmed defensively.
- **The Map turns green the moment indexing finishes.** After a successful re-index the Map (and its
  graph + tables) could stay stuck on the old orange "in progress" view until you reloaded. Finishing
  a job now refreshes the Map automatically.

### Added

- **Plain-language help throughout.** A "What is this?" explainer on the Map and tooltips across the
  UI define the jargon in everyday terms — *chunks* ("the small searchable pieces your files are split
  into"), *summaries*, *coverage*, and what green / orange / grey mean — because the audience is
  everyone, not just engineers.
- **A resizable, readable sidebar.** Drag the divider to widen the file tree (your width is
  remembered), and the row-action buttons now tuck away until you hover a row, so long folder names
  are no longer hidden behind them.
- **Updates that explain themselves.** "Check for Updates…" now shows the new version **and what
  changed** (the release notes), asks once, then downloads and restarts into the new version — like a
  normal app, instead of a bare "an update is available." Release notes are sourced from the
  changelog automatically.
- **One-click command-line tool install.** A new **"Install command-line tool"** item in the app and
  tray menus downloads the matching `indexa` CLI for this release and puts it on your `PATH`, so your
  terminal `indexa` stays in sync with the desktop app.

## [0.28.2] — 2026-06-14

A hygiene patch: the summary queue no longer fills with dead rows, and `prune` tells the truth.

### Fixed

- **The queue no longer reports a backlog it can't work.** Build-artifact / deleted-file paths with
  no `entries` row used to sit `pending` forever (one index showed "900 pending" where ~685 were dead
  rows), and the worker could even waste a model call summarizing a `.git/` file. Now the drain
  **deletes** a claimed row whose path is no longer a live entry instead of summarizing it, so the
  queue self-cleans on the next run; `status` (and the engine bar) count only entry-backed `pending`
  work and surface the rest as a `stale` hint ("N stale → run `indexa prune`").
- **`indexa prune` now reports the queue and classification rows it removes** (it already deleted them;
  it just under-reported — only chunks + summaries). On a real index this surfaced 685 dead queue rows
  and 9,091 orphan chunks that prune had been silently clearing.

### Changed

- **The summarize-enqueue path skips non-entry paths** (a watch event under a skipped build dir, say),
  so the queue can't re-accumulate un-processable rows. Bypassed for an entry-less
  `deep`/`summarize`-without-`scan` index (entries remain optional by design).

## [0.28.1] — 2026-06-13

A correctness patch: stop falsely reporting that there's no RAM to run a model.

### Fixed

- **Memory budget no longer counts the macOS compressor as "used."** On a busy Mac, Indexa could
  report a tiny budget and refuse to load a model ("too much RAM used") while macOS itself showed
  plenty free and ran the model fine. The budget was computed from `total − used_memory()`, and
  sysinfo 0.39's `used_memory()` on macOS *includes* the compressor (compressed memory, often 10+ GB).
  It now uses the OS's own **available-memory** figure (active + inactive + free — what
  `memory_pressure` reflects), which is the basis for the model-fit check, the engine bar, the
  watchdog, and `indexa doctor`. Measured on a 36 GB machine: the reported budget went from ~0.5 GB
  to ~10.5 GB, and the local model loads and answers as expected.
- **Linux desktop build.** The Dock-reactivation handler used a macOS-only Tauri event
  (`RunEvent::Reopen`) without a platform guard, breaking the (experimental) Linux desktop build; it's
  now `#[cfg(target_os = "macos")]`-gated.

### Added

- **`indexa doctor --apply-ollama-env`** — opt-in: applies the recommended Ollama server settings
  (`OLLAMA_KEEP_ALIVE=30s`, `OLLAMA_MAX_LOADED_MODELS=1`, `OLLAMA_NUM_PARALLEL=1`) via `launchctl
  setenv` on macOS (prints the `export` lines elsewhere), so models unload promptly and don't stack.

## [0.28.0] — 2026-06-13

"Better in every sense": one broad polish release — discoverable desktop updates, a self-healing
index, a token-savings dashboard, a responsive + keyboard-navigable web UI, and CLI/MCP ergonomics.

### Added

- **Token-savings "Impact" dashboard.** Settings → Impact shows the tokens Indexa saved your AI tools
  this week and a per-tool breakdown (`ask` / `search` / `get_summary` / `read_file` / …), with the
  honest ≈4 bytes/token caveat. New `GET /api/impact`; `indexa status --json` gains a per-tool
  `by_tool` array.
- **Discoverable desktop updates.** A native macOS **app menu** (Indexa → About · Check for Updates… ·
  Quit ⌘Q; Edit copy/paste/select-all; Window) — "Check for Updates…" was previously reachable only
  from the tray icon. Launch now *checks* for an update without silently downloading it, so reopening
  the app no longer forces a surprise restart; clicking the Dock icon re-shows a hidden window.
- **`indexa completion <bash|zsh|fish|powershell|elvish>`** — generated from the live CLI definition.
- **`indexa mcp install` auto-detects** installed clients (claude-code / claude-desktop / cursor /
  vscode) when run with no `--client`, and configures each one found.
- **Three new MCP tools (39 → 42):** `query_config` (effective config, never secrets),
  `list_files_by_category` (classification category → files), `get_chunk_context` (a file's indexed
  chunks, or the neighbors of a search hit). Plus `offset` pagination on `list_open_decisions`.
- **Persistent coverage legend** (● built · ◐ partial · ○ none · ✗ failed) under the sidebar header —
  the glyphs were tooltip-only before.
- First-run "next steps" after a build now offers **Export** alongside Ask and Browse.

### Changed

- **The index self-heals.** A rescan now auto-prunes the orphaned chunks/summaries left behind when
  build artifacts are removed — no manual `indexa prune` needed — and the default skip rules guard
  more build/cache directories (`build` / `dist` / `vendor` / `Pods` next to a manifest).
- **Mobile / responsive web UI.** The CSS had essentially no breakpoints; now the sidebar collapses
  into a slide-out drawer (hamburger + scrim) at ≤1024px, and the workspace stacks with 44px tap
  targets at ≤768px.
- **Keyboard navigation + accessibility.** Arrow-key navigation over the folder tree (WAI-ARIA tree
  pattern, roving tabindex); the code-graph nodes are focusable with relationship `aria-label`s. The
  long Settings drawer is now a collapsible accordion (first two sections open). A dark-theme contrast
  audit confirmed every text-on-surface pair already clears WCAG AA — no token change needed.
- **Richer `--help`** with examples on `ask` and `classify`; the MCP tool count in README / CLAUDE.md /
  docs now reads **42** (a build-time guard keeps it honest).

### Fixed

- The desktop "Check for Updates…" command was effectively hidden (tray-menu only) and the on-launch
  auto-download produced a "restart to update" prompt the next time the app opened — both addressed by
  the native menu + check-then-ask flow above.

### Internal

- Web-handler tests (ask scope/agentic/empty, export, stats, review batch, the new `/api/impact`),
  parser error-case tests (malformed PDF/EPUB/media → graceful stub, size cap), and a **macOS
  desktop-build CI job** on PRs (the Tauri crate is excluded from `cargo --workspace`, so breakage
  used to surface only at release).

## [0.27.0] — 2026-06-13

"Context that answers": make Ask actually answer about what you're looking at.

Selecting `CLAUDE.md` and asking *"what is this file?"* used to return PNG **icon** files and
*"the context only lists filenames and sizes"* — because Ask searched the whole index with no idea
which file you had open, and content-free image placeholders out-ranked real text. Three fixes turn
that around, all working on your existing index with no re-index.

### Added

- **File-aware Ask.** Selecting a file or folder now auto-scopes Ask to it, shown as a removable
  **"Asking about: &lt;name&gt; ✕"** chip (clear it for a whole-index question). The Context summary
  gains an **"💬 Ask about this file"** button that bridges straight into the scoped Ask — present
  even before a file is summarized, since scoped answers work on its raw content. When a single-file
  scope returns little, Ask offers to **broaden to the folder** rather than silently going global.
  (`scope` rides the request the same way `indexa ask --scope` and MCP `ask {scope}` already do.)
- **"Context not built yet" banner.** When an index is embedded but not yet summarized, a dismissible
  banner explains answers are falling back to raw chunks and offers a one-click **Build context** —
  instead of silently returning thin results. Auto-hides once summaries exist.

### Changed

- **Content-free image/binary stub chunks are excluded from retrieval.** Placeholders like
  `File: logo.png` (emitted for images without captions) no longer surface as answer sources or crowd
  out real content — filtered in the search SQL and guarded again at synthesis. Fixes existing indexes.
- **The app opens on your Context, not a blank Ask box.** A returning user lands on the file tree +
  summary (Ask is one click away), so there's always something to orient to.
- **Image captioning defaults to gemma3** (the Google multimodal model already pulled for summaries)
  instead of a separate ~8 GB vision model — captioning works out of the box when you enable it, with
  no extra download and within the existing memory budget. Set `[parsers.image] model` to override.
- **Plainer labels.** "Build deep context" → **"Index for search"**; the Context welcome now says
  **"Select a file or folder to see what it is."** Sidebar row actions (scan / index / summarize /
  remove) are revealed on hover, keyboard focus, **and** when a row is selected — no longer
  hover-only — and are keyboard-reachable.

## [0.26.0] — 2026-06-13

"Honest memory": tell the truth about RAM, and finish the loose ends.

The engine bar used to read like a generic system monitor — "most of your RAM is used" — which on
macOS is always true (the OS keeps memory resident as reclaimable cache) and told you nothing about
whether Indexa could load another model. It now reports the number the resource engine actually
reasons about: how much room there is for a new model above the keep-free band. And the one piece of
memory Indexa genuinely owns — its resident Ollama models — now has a button to release it.

### Added

- **Engine bar "free for a new model" memory readout.** The bar shows `used · free` where *free* is
  the model **budget** (`total − used − headroom`), not OS-free RAM, with a tooltip that explains
  *used* excludes reclaimable cache and *free* is room above the keep-free band. Pressure now reads
  **memory ok / tight / low**, derived from that budget — the old swap-percentage wording (which was
  misleading on a healthy machine) is gone from the engine bar and the warnings panel. See the new
  *"What the engine bar's memory numbers mean"* section in `docs/methodology.md`.
- **"Free models" button** (`POST /api/engine/release`). Unloads Indexa's **own** loaded local
  models (Ollama `keep_alive=0` eviction) on demand — explicitly **not** a system purge; it cannot
  touch other processes' memory, and the RAM frees as Ollama evicts. No-op and safe for cloud
  providers.
- **Token-savings widget** in the engine bar — "~N tok/wk" with a tooltip showing the served-vs-
  whole-file basis (`≈4 bytes/token, estimated`). Hidden until there's a week of usage to report.
- **Web batch-answer for the review inbox.** A "Batch answer…" control answers every open question
  of a type under a folder at once (blank = all folders), mirroring the CLI's
  `review answer --type … --under … --choose …`. Confirms before applying; only batch-safe answers
  per type are offered (the shared `decisions::batch_answer_refusal` guard is now the single source
  of truth for both CLI and web).

### Changed

- **`indexa related` and the web Map graph now show resolution tiers.** `related` gained a **Tier**
  column (same-file / import / same-dir / bare) in both the table and `--json`; the Map graph styles
  scoped edges solid and bare-name edges dashed/muted, and reports the bare-name caveat only on the
  bare remainder. In `strict` mode the graph now says *"strict (bare-name dropped)"* rather than
  claiming *"all scope-resolved"* — bare edges were filtered out, not resolved. (Completes the v0.25
  scoped-resolution surfacing.)

## [0.25.1] — 2026-06-13

A critical desktop-updater fix. The macOS desktop app's embedded **web** "Update now" button
(Settings → Software Update) ran the CLI's *binary* self-replace against its own `.app` bundle —
downloading the headless `indexa-<arch>-apple-darwin` CLI binary, swapping it over the GUI Mach-O,
and ad-hoc re-signing it. That stripped the Developer-ID signature + notarization, leaving a
quarantined ad-hoc bundle that Gatekeeper refuses to launch (the app showed *"Updated to v… —
relaunching…"* and never came back). The Tauri menu-bar updater was never affected.

### Fixed

- **The desktop app no longer exposes a binary self-replace updater.** It stops setting
  `INDEXA_WEB_ALLOW_UPDATE`, so the web "Update now" button is gone in desktop mode; updates flow
  only through the menu-bar **"Check for Updates…"** (Tauri's notarized-`.app` updater).
- **`indexa update` / `crates/update` refuses to self-replace inside a macOS `.app` bundle** (or
  when `INDEXA_DESKTOP=1`) — a hard guard that fails before any download, so no caller can corrupt
  a bundle this way again.
- **`POST /api/update/apply` refuses in desktop mode** (HTTP 403, points to the menu-bar updater),
  and `GET /api/update/check` now returns a `desktop` flag so the web UI hides the button.
- **The desktop's post-update ad-hoc re-sign now fails closed** — it only re-signs a bundle it can
  positively confirm is ad-hoc/unsigned, never a Developer-ID/notarized one.
- **CI asserts the desktop bundle is Developer-ID signed, notarized, and stapled** on every signed
  release (an un-stapled bundle would fail the updater's offline launch).

> **If your desktop app is already broken** (won't open after clicking the web "Update now"):
> reinstall from the notarized DMG — your index data in `~/Library/Application Support/dev.indexa.Indexa/`
> is untouched. From a working v0.25.0+, updating to v0.25.1 via the **menu-bar** "Check for
> Updates…" is safe; do not use the web button until you're on v0.25.1 (where it's removed).

## [0.25.0] — 2026-06-11

"Deep Accuracy": earn back the asterisks.

### Added

- **Scoped call resolution.** The D2 call graph resolves each call through evidence tiers —
  **same-file** (an intra-file helper named like a popular symbol no longer fans out repo-wide),
  **import-linked** (relative JS/TS imports, Rust `crate::`/`super::` paths, dotted Python
  modules), **same-dir** (proximity), then labeled **bare-name** fallback — at query time, on
  existing indexes, no re-index needed. `who_calls` groups callers by tier; `code_graph`,
  `blast_radius`, `indexa graph`, and `related_files` report scoped vs bare counts, and the
  bare-name caveat now applies only to the bare remainder. `strict` drops the bare tier entirely.
  Heuristic import-string matching, not semantic analysis — what does and doesn't resolve is
  tabled in [methodology](docs/methodology.md). On the test fixture: 11 bare edges → 6 scoped,
  zero true edges lost.
- **Decision Ledger phase 3.** Three new question types: **summary drift** (a regeneration of
  unchanged content that disagrees with the old summary — keep new or restore old, both abstracts
  quoted), **language fallback** (files whose chunks lost language detection), and **symbol
  ambiguity** ("which definition of `parse` is authoritative?") — answering the last one now
  actually narrows `who_calls`/`blast_radius` to the pinned definition. The web review drawer
  gains **time-travel**: per-question history chains with one-click *restore this answer*
  (shared `revert_decision` core with the CLI).
- **Experimental Linux desktop build** — AppImage + .deb artifacts on releases (unsigned, no
  auto-update yet; the job cannot block the CLI release).
- `indexa status --deep`-era docs caught up: ROADMAP records today's five releases; USAGE.md
  explains why `report` (ask digest) and `insights` (index analytics) both exist.

## [0.24.0] — 2026-06-11

"Always Current": the index never lies about freshness.

### Added

- **Incremental re-summarize.** `summaries.source_hash` is now real (full-content SHA-256 for
  files, a Merkle-style roll-up over child hashes for directories) and gates the LLM: a refresh
  skips every file whose bytes are unchanged — `indexa summarize` now reports
  *"N summaries written, M unchanged (skipped)"* — and re-rolls a directory only when its subtree
  actually changed. Stale candidates are found by an mtime pre-filter (timestamped at the START of
  each summarize run, so edits landing mid-run aren't lost) and changed files re-pend their
  ancestor roll-ups automatically. The web **Regenerate** action clears stored hashes first, so an
  explicit regenerate always re-runs the AI (model/prompt switches included). Freshness limits
  (mtime-preserving copies) are documented in
  [methodology](docs/methodology.md#freshness-limits-of-incremental-re-summarize).
- **Near-duplicate detection without the 5,000-file cap.** Above ~2,000 summarized files,
  candidate pairs come from deterministic locality-sensitive hashing with exact cosine
  verification — linear-ish in index size, no silent truncation. Disclosed as approximate
  (borderline pairs can be missed; exact-content groups stay exhaustive) in the CLI, web,
  MCP tool description, and [methodology](docs/methodology.md#near-duplicate-detection-accuracy).
- **Decision Ledger: archive questions.** Top-level folders untouched for a year become a
  question — *"~/old-project hasn't changed in 400 days — archive it?"* — where **archive** keeps
  everything indexed and searchable but down-weights it in results (reversible), and
  **keep active** asks again only after another ~3 months of inactivity. Insights gains
  **"Don't ask about this"** on duplicate clusters and stale entries: a sticky dismissal recorded
  through the same ledger (returns only if the evidence changes). `indexa prune` now also GCs
  old dismissed/expired questions.
- **Web smoke test in CI.** A zero-dependency headless-Chrome harness (scripts/web-smoke.mjs)
  boots a fixture index, drives the real UI over CDP, and fails on any console error — running on
  every PR.

## [0.23.0] — 2026-06-11

"Measure It": the pitch becomes a measurement.

### Added

- **Token-savings telemetry — the pitch, measured.** Every content-serving retrieval call (`ask`,
  `search`, `get_summary`, `read_file`, across CLI/web/MCP) now records what it served vs. the
  full on-disk size of the files behind it. `indexa status`, MCP `get_stats`, and the web header
  report: *"This week Indexa served 12.3 KB where whole-file context would have been 4.2 MB —
  roughly 1.1M tokens saved (estimated at ≈4 bytes/token)."* The counterfactual is an estimate and
  is documented as one — see the new ["What tokens saved means"](docs/methodology.md#what-tokens-saved-means)
  section. UI navigation (the sidebar path filter) deliberately records nothing.
- **Answer confidence.** `ask` now labels each answer **high / medium / low** from the retrieval
  evidence (hit count, fusion-score strength, keyword+semantic corroboration, drop-off), with the
  basis stated: `confidence: medium — 4 moderate matches`. Shown in the CLI (+ `--json` fields,
  inputs under `--explain`), the web chat, and the MCP `ask` response. A heuristic, not a
  calibrated probability — [documented](docs/methodology.md#what-retrieval-coverage-on-an-answer-means).
- **`indexa status --deep` — the index health report.** Coverage at a glance: % files chunked,
  % chunks embedded (with an explicit "dense search can't see them" callout when short),
  summary coverage, summaries older than their file, queue depth, open review questions, and
  last-indexed per root. JSON via `--json`.
- **`indexa eval` — retrieval regression harness.** Golden-questions JSON → hit rate, MRR, and
  citation precision against the same retrieval `ask` uses (LLM-free; sparse mode needs no
  embedder). `--min-hit-rate` turns it into a CI gate. This is the measuring stick future
  retrieval changes (tree-sitter call resolution) must move before they ship.
- **`indexa mcp install --client claude-code|claude-desktop|cursor|vscode`** — one-shot MCP
  registration. JSON-config clients get a safe merge (other servers untouched, `.bak` of the
  original, write-temp-then-rename, invalid JSON refused rather than clobbered); Claude Code
  delegates to `claude mcp add`. `--dry-run` previews. Bare `indexa mcp` still runs the stdio
  server, unchanged.

## [0.22.0] — 2026-06-11

"The Ledger": Indexa asks instead of guessing — and remembers your answer.

### Added

- **The Decision Ledger.** Indexa now records the judgment calls indexing wasn't confident enough
  to make alone — and asks you, instead of guessing. Uncertain folder classifications (Tier-0
  confidence in the 60–80% band) and duplicate clusters ("which copy is canonical?") become
  **questions in a review inbox**; your answers are recorded durably with provenance, applied
  immediately (classification confirmed; non-canonical copies down-weighted to 0 in search,
  reversibly), and **remembered as revision chains**: when a folder's contents later change enough
  to contradict your answer, Indexa **re-asks** — quoting what you said and when — and never
  silently overrides you. Reach it from all three surfaces:
  - **CLI** — `indexa review list / show / answer / dismiss / history / revert / scan / gc`
    (answer by option number: `indexa review answer 12 2`); batch answers with
    `--type … --under <dir> --choose …`.
  - **Web** — a review drawer (envelope icon, live count badge) where questions are cards with
    one-click answers.
  - **MCP** — 5 new tools (`list_open_decisions` / `get_decision` / `answer_decision` /
    `dismiss_decision` / `decision_history`, **39 tools** total): an agent can relay Indexa's
    questions to you mid-session and record your answer.
  Question fatigue is engineered against: confident automatic judgments stay out of the ledger,
  open questions are capped (`[review] max_open`, default 50; `max_new_per_scan`, 20), dismissal
  is sticky (a dismissed question returns only when its evidence changes), questions whose
  evidence leaves the index expire automatically, and the budget is spent on your
  highest-priority questions first (re-asks of your own answers outrank fresh suggestions).
  Everything is local — the ledger is your index learning your judgment, on your machine.
- Pre-ledger classification answers (confirm/ignore) are imported automatically as decided
  ledger revisions the first time `classify` runs, so re-asking works for them too.

### Changed

- The MCP server crate is split into family modules (retrieval / graph / packs / curation /
  insights / admin / review) behind golden contract tests — no tool behavior changed.

## [0.21.0] — 2026-06-11

"Truth & Trust": every claim the project makes — in docs, in tool output, in summary rows — is now
either true or build-breaking.

### Added

- **Saved searches everywhere.** The `saved_queries` table (CLI-only since v0.20) is now reachable
  from the web Ask bar (a recall dropdown + a one-click ☆ save) and from agents via a new
  `list_saved_queries` MCP tool (**34 tools** total).
- **Summary provenance.** Every summary row now records *how* it was made: the adapter
  (`provider`), the refinement passes actually run (`passes`), and whether a lighter model was
  auto-substituted for the configured one (`fallback`). Substrate for the upcoming decision ledger.
- **Honesty caveat in code-graph results.** `blast_radius` and `code_graph` responses (MCP) and
  `indexa graph` output now carry the bare-name-matching caveat inline, so agents reading result
  bodies see the approximation warning — not just readers of the tool docs.
- **Grouped CLI help.** `indexa --help` presents the 28 commands as five ordered families
  (Core · Manage · Analyze · Pipeline · System) and the quick-start headlines one-command
  `indexa index`.
- **docs:** a real [Troubleshooting guide](docs/TROUBLESHOOTING.md); per-client MCP setup
  (Claude Code / Claude Desktop / Cursor / VS Code) in the MCP how-to; a contributor map in
  `docs/architecture.md`; ANN opt-in recipe in USAGE.md; an illustrative token-savings worked
  example in the README.

### Fixed

- **Summary `model` column lied under auto-downgrade.** When the memory-fit pre-flight (CLI) or
  the web "ask me first" popover substituted a lighter model, the summary row still recorded the
  *configured* model. The substituted models are now recorded, with `fallback = 1`.
- Stale docs reconciled with the code: MCP tool count (29 → 34), CHANGELOG release sections for
  v0.20.x backfilled (including both maturity sprints), COMPETITIVE.md re-baselined to v0.20.1
  with a staleness header, wrong DB filename/paths in USAGE.md corrected.

### Internal

- **The doc-drift class is now CI-enforced:** a golden MCP tool list + contract calls
  (`crates/mcp/golden_tools.txt`), a test failing the build when any doc's "N tools" claim
  drifts from the code, a release gate requiring a CHANGELOG section for the tag, and an
  offline Markdown link check on docs PRs.
- HTTP retry/backoff consolidated into a new `indexa-http-util` crate (was duplicated across
  `indexa-llm` and `indexa-embed`).

## [0.20.1] — 2026-06-11

The first **working** Developer-ID-signed, notarized, universal macOS release — v0.20.0's desktop
binaries crashed at launch (see below). Coming from v0.19.0 or v0.20.0, install this one manually;
auto-update resumes from here.

### Fixed

- **Desktop app statically links libpcre2** (`PCRE2_SYS_STATIC=1` via `.cargo/config.toml`). The
  v0.20.0 binary dynamically linked Homebrew's `libpcre2` (pulled in by hyperpolyglot → pcre2 →
  pcre2-sys); the hardened runtime's library validation rejected the different-Team-ID dylib and the
  app died at `dyld` before reaching `main`. (#189)
- **Updater publishes under the default per-arch targets** (`darwin-aarch64` / `darwin-x86_64`)
  instead of a custom `darwin-universal` key, so existing installs actually find the update
  artifact. (#188)

## [0.20.0] — 2026-06-10 — **withdrawn**

> **Withdrawn:** the macOS desktop app in this release crashed at launch (dynamically linked
> Homebrew `libpcre2` rejected by hardened-runtime library validation). Every feature below shipped
> here and works; use **v0.20.1** for working binaries.

### Added

- **Agentic multi-step `ask`.** `indexa ask --agentic` (also MCP `ask` `agentic: true`, and an
  "Agentic" checkbox in the web chat) runs a bounded *plan → search → refine* loop: search, ask the
  model whether an important part of the question is still uncovered, take one focused follow-up query,
  repeat, then synthesize from the merged context. Helps on compositional questions whose pieces live
  in different files. Opt-in (`--max-steps` 1–5, default 3) and **fails open** to ordinary one-shot
  retrieval if the model won't emit the search/done actions. The web UI streams each hop as a live
  "🔍 searching" chip above the answer.
- **Weighted PageRank centrality for the code graph.** Every file in the signature graph now carries a
  centrality score; the Map "Graph" view **sizes nodes by centrality**, and `indexa graph` + the
  `code_graph` MCP tool list the most-central hub files. (Inherits the bare-name-match caveat of the
  call graph — an approximate "read these first" signal, not a dependency analysis.)
- **Universal macOS desktop build.** The desktop app ships a single `.dmg`/`.app.tar.gz` that runs
  natively on both Intel and Apple-Silicon Macs (`--target universal-apple-darwin`, published under the
  `darwin-universal` updater key).
- **Developer ID signing + notarization for the desktop app** (release pipeline wired). When the Apple
  signing secrets are present the build is Developer-ID-signed + notarized (Gatekeeper-clean, no ad-hoc
  re-sign needed); it falls back to ad-hoc signing otherwise. See `docs/signing.md`.

- **`indexa prune`** — garbage-collect orphaned index rows (chunks/summaries whose path has no
  `entries` row) left behind after a root is removed or re-pointed. `--dry-run` previews the count;
  no-ops on a deliberately entry-less index (`deep`/`summarize` without `scan`).
- **Scanner honors `.gitignore` + a config `[scan] ignore` list.** On top of the built-in skips
  (`node_modules`/`target`/`.venv`/…), `scan`/`deep` now skip files matched by the scan root's
  `.gitignore` (default on; `[scan] respect_gitignore`) and any extra gitignore-style patterns in
  `[scan] ignore` — so project-specific build/output dirs stay out of the index.

Two maturity sprints (#169–#175, #176–#184) also landed in this release:

- **`indexa snapshot`** — portable index snapshots bundling summaries + graph + weights. (#184)
- **`indexa report`** — a multi-question digest document synthesized from the index. (#183)
- **Saved searches** — `indexa saved add/list/run/rm` for named, reusable ask queries. (#182)
- **`indexa related` + dependency-cycle detection** in the code graph. (#181)
- **Insights: largest files + language breakdown** across CLI + MCP. (#180)
- **`indexa search` primitive** (hits only, no synthesis), **pack rename**, and an MCP `prune`
  tool. (#179)
- **Export: token-count estimate** + `--include-weights` / `--include-graph`. (#178)
- **Truncation marker + wider summary-boost scan** in retrieval. (#177)
- **Worker `--auto-reindex`** — refresh stale roots before draining the queue. (#175)
- **Strict resolution mode for the code graph** — a precision filter that keeps only
  unique-definition call edges. (#174)
- **Web a11y pass** — tablist arrow-keys, live regions, modal focus traps, AA contrast. (#172)
- **`--json` for `ask`/`status`, `ask --explain` retrieval trace, `doctor` Ollama probe.** (#170)

### Fixed

- **`deep` now self-heals a partially-embedded index.** A file whose chunks were stored without a
  vector (e.g. an embed failure during an Ollama outage) was treated as "current" and skipped on every
  later `deep`, staying invisible to dense search. `deep` now re-embeds a file unless *every* chunk has
  a vector — so a plain re-run fixes a broken index (no manual `rm -r` needed).
- **Repaired app-wide broken CSS design tokens.** Several rules referenced custom properties that don't
  exist (`--surface2`/`--surface3`/`--fg`) and silently computed to transparent — the Export button,
  the Map active sub-tab, breadcrumb/root-pill hovers, tooltips, and the export dropdown were broken
  (notably in light theme). Added `--surface-3` + `--accent-muted` and reconciled every reference; plus
  a subtle treemap fade-in on render.
- The web `ask` path no longer silently drops the `[retrieval] use_weights` setting.
- **Hardening:** Claude-Code adapter timeout, Ollama retry parity in the LLM client, an
  embedding-dimension guard on `deep` (#176); poison-safe job mutexes + surfaced
  previously-swallowed job errors in the web server (#169); integration tests for web handlers and
  MCP tools (#171).

## [0.19.0] — 2026-06-05

### Fixed

- **Desktop auto-updater now survives macOS 26.** After the in-app updater replaces the
  `.app` bundle, the desktop app re-signs it (`codesign --force --deep --sign -`) before
  restarting — the macOS 26 Code Signing Monitor otherwise invalidates the trust record on
  an in-place overwrite and the updated app would be killed on launch. This mirrors the
  v0.17 CLI fix and is the root cause of the desktop app lagging behind releases. Self-heals
  from this version onward.
- **`indexa rm -r <dir>` / `DELETE /api/entry` no longer remove sibling paths.** Subtree
  deletion matched a bare string prefix, so removing `/proj` also dropped `/projector/…`
  from the index. It now matches the path itself plus `<path>/…` only — siblings are spared.
  (Index-only; recoverable by re-scan, but a real correctness bug.)
- **Watcher surfaces embedding failures.** A live-watch chunk whose embedding failed was
  stored silently without a vector (degrading dense search invisibly); it now logs a warning.
- **Corrected a misleading schema/`upsert_entries` comment** that referenced FK CASCADE
  constraints which don't exist (follow-on to #155).

## [0.18.0] — 2026-06-04

### Added

- **Signature graph — interactive call-graph visualization.** The code-relationship
  graph (previously text-only over MCP) is now a visual: who-calls-whom across your files.
  - **Web UI:** a new "Graph" sub-view in the Map tab — a force-directed view of the
    file-to-file call graph (hand-rolled vanilla SVG, no libraries). Pick a scope, hover a
    node to highlight its callers/callees, see node/edge counts and a truncation banner.
  - **Store:** `Store::code_graph(prefix, max_edges)` joins `calls` edges to `defines` edges
    (file A → file B when A calls a function B defines); edge weight = shared symbol count.
    Generic names defined in >25 files (`new`, `from`, …) are excluded as noise, which also
    bounds the JOIN on a whole-disk index. Scope is directory-normalized (`/proj` does not
    match `/projector`).
  - **REST API:** `GET /api/graph?scope=<path>&limit=<n>` (runs in `spawn_blocking` on a
    fresh connection — never holds the shared store mutex).
  - **MCP:** new `code_graph` tool (29 tools total).
  - **CLI:** `indexa graph <dir> [--limit N]` prints the call-graph edge list.
  - Call edges use bare-name matching (case-sensitive, 1-hop, Rust/Python/JS/TS/Go/Java) —
    labeled honestly in the UI; see `docs/methodology.md`.

### Changed

- Docs refreshed to the current feature surface: `CLAUDE.md` gains a feature/CLI/MCP summary
  and a web-UI build note; `README.md` MCP tool count corrected (10 → 29) and graph viz added;
  `ROADMAP.md` marks the signature graph shipped.

## [0.17.0] — 2026-06-04

A maturity pass: fixes bugs found in the v0.16 audit, adds the missing test
coverage, and polishes the new-user experience.

### Fixed

- **Video captioning now works when only `parsers.video.caption` is enabled.**
  The vision-model handle was built only for image captioning, so enabling just
  video captioning silently extracted frames and captioned nothing. The handle is
  now built when either image or video captioning is on, with a loud warning if no
  vision model is available.
- **Duplicate-detection no longer blocks other requests.** `/api/insights/duplicates`
  ran its O(n²) near-duplicate scan while holding the shared store mutex, stalling
  every other API call. It now runs on a fresh, short-lived connection inside
  `spawn_blocking`. Near-duplicate detection is also capped at 5000 candidate files
  to bound the scan on whole-disk indexes.
- **`indexa` with no arguments now prints help** instead of a bare usage error.
- **`indexa status` shows the running version** (`Indexa: vX.Y.Z`) as its first line.
- **`indexa weight set` warns when a path-like target does not exist on disk** — the
  weight is still stored, but a likely typo is surfaced.
- **JS bundle: renamed the duplicated `esc()` helper** in `17-weights.js` to `escW()`
  to avoid silently overriding the identically-named helper in `16-context-packs.js`.
- **`importance_weights` boost is no longer per-hit-SQL.** `boost_with_weights` now
  pre-loads the (small) weights table into memory once per query instead of firing
  up to ~200 SQL round-trips for a typical answer.

### Added

- **Web Insights: configurable thresholds.** The Insights panel now has "older than N
  days" (stale) and "last N days" (weekly diff) inputs instead of hardcoded 365/7.
- **Tests** for all v0.16 store logic: importance weights (set/resolve/boost/recency)
  and insights (exact + near duplicates, stale, weekly diff), plus the parser
  `Registry::register()` plugin-dispatch contract.

### Changed

- `first_indexed_at` is now part of the base `entries` schema (was migration-only),
  so fresh databases skip the column-add migration entirely — eliminating a
  concurrent-open race.
- `if-addrs` moved to `[workspace.dependencies]`; removed the redundant `tempfile`
  dev-dependency in `indexa-parsers`.
- README "What's coming" updated — v0.8 / v0.10 moved to shipped; next milestones are
  the mobile companion, plugin marketplace, and graph visualization.

## [0.16.0] — 2026-06-04

### Added

- **v0.8 Importance weighting.** Per-file, per-directory, and per-category boosts
  applied multiplicatively to search RRF scores.
  - `indexa weight set/get/list/delete/suggest/apply` — full CLI surface.
  - `--auto` recency-based suggestions (files modified in last N days).
  - REST API: `GET/POST/DELETE /api/weights`, `GET /api/weights/suggest`.
  - Web UI: "Importance Weights" section in Settings drawer.
  - MCP tools: `list_weights`, `set_weight`, `delete_weight`.
  - Config: `[retrieval] use_weights = true` enables the boost in Q&A.

- **v0.10 Insights.** Analytical reports over the index.
  - `indexa insights duplicates [--exact] [--threshold]` — exact (content hash)
    or near-duplicate (embedding cosine) cluster detection.
  - `indexa insights stale [--days 365]` — directories not modified for N days.
  - `indexa insights diff [--days 7]` — what was added or modified this week.
  - REST API: `GET /api/insights/duplicates`, `/stale`, `/diff`.
  - Web UI: "Insights" section in Settings drawer (run on demand).
  - MCP tools: `insights_duplicates`, `insights_stale`, `insights_diff`.
  - DB migration: `entries.first_indexed_at` — stable discovery timestamp
    (never reset on rescan; enables "what was added this week" queries).

- **Video frame captioning (opt-in).** `[parsers.video] caption = true` samples
  frames via ffmpeg and captions each with a local Ollama vision model.
  Requires `ffmpeg` on PATH; configurable `fps_sample` (default 0.5) and
  `max_frames` (default 8). Video toggle added to the Advanced Features Settings
  UI alongside image and audio options.

- **Plugin SDK — extensible parser registry.**
  - `indexa_parsers::Registry` struct with `new()`, `register(Box<dyn Parser>)`,
    and `parse()`. Custom parsers inserted before built-ins take precedence.
  - All plugin types (`Parser`, `Chunk`, `Extracted`, `Edge`) are public stable API.
  - `crates/parsers/examples/custom_parser.rs` — minimal reference implementation.
  - Existing `parse()` / `parse_guarded()` free functions unchanged.

- **LAN serve.** `indexa serve --host 0.0.0.0` exposes the web UI on all interfaces
  for mobile or second-device access. Prints all local IPv4 addresses on startup.
  Desktop app always binds to 127.0.0.1 (no change). Config: `[serve] host`.

### Fixed

- **`upsert_entries` non-destructive upsert.** Replaced `INSERT OR REPLACE INTO
  entries` (which DELETE+INSERTs on conflict, resetting the implicit rowid and
  breaking any future FK CASCADE) with `ON CONFLICT(path) DO UPDATE SET …`.
  The row identity is now stable across rescans.

## [0.15.0] — 2026-06-04

See PR #147. MCP completeness (22 tools), pack scoped search, `indexa doctor`
integrity/queue/codesign checks, `idx_edges_from` index, CHANGELOG v0.14.0 entry.

## [0.14.0] — 2026-06-04

### Added

- **Context Packs (v0.9).** Named, cross-directory context bundles — group files from
  anywhere on your disk into a topic and export them as one self-contained XML, Markdown,
  or JSON file for any AI tool.
  - `indexa pack create "Auth" [--auto] [--yes] [--limit N]` · `add` · `remove` · `list`
    · `show` · `export` · `delete`
  - `--auto` embeds the pack name, finds semantically related summaries via
    `summary_cosine_search`, shows candidates with a confirm prompt, and falls back to
    BM25 keyword search when embeddings are unavailable.
  - **REST API** — 8 new endpoints: `GET/POST /api/packs`, `DELETE /api/packs/:name`,
    `GET/POST/DELETE /api/packs/:name/paths`, `GET /api/packs/:name/export`,
    `POST /api/packs/suggest`. Duplicate name → 409; missing pack → 404;
    unsummarised pack → 422.
  - **Web UI** — "Context Packs" section in the Settings drawer: pack list with path
    counts, create form, per-pack edit/export/delete, inline path editor with
    add/remove, XML/Markdown/JSON export download.
  - **MCP tools** — `list_packs`, `get_pack`, `export_pack` (10 → 13 tools).
  - **12 store-layer tests** covering the full CRUD surface.

## [0.13.0] — 2026-06-04

### Added

- **`indexa index` — one-shot context build.** `indexa index <path>` replaces the
  three-step pipeline (`scan` → `deep` → `summarize`) with a single command. Each phase
  prints a "Phase 1/2/3" progress header. Supports `--embed-model`, `--mode`, `--passes`.
- **Job cancel button.** A ■ Cancel button now appears in the Activity drawer job
  detail pane for running jobs. Calls `DELETE /api/jobs/:id`; disables immediately on
  click to prevent double-cancel; hides once the job transitions out of running.
- **Context Coverage Map.** The Map tab treemap now sizes cells by **chunk count** (not
  bytes) and colors them by coverage state: ● green = built, ◐ orange = in progress,
  ✗ red = failed, ○ grey = not built. A root picker prevents a large root (e.g. `/`)
  from swallowing everything into one block. The Table sub-view shows a coverage
  breakdown (built / in-progress / failed / not-built counts + % of folders covered).
- **Export toolbar button.** "Export ↓" added to the workspace toolbar (right of the
  Context / Map / Ask tab row) so the export action is always reachable without first
  opening a folder summary panel.
- **MCP `search` now does real content search.** Upgraded from a path-LIKE query
  (`store.search_paths`) to BM25 + vector hybrid retrieval (`hybrid_search`). Returns
  chunk-level results: file path, heading, 120-char snippet. Adds optional `scope`
  parameter for subtree filtering.
- **`indexa serve` enables web update button.** `INDEXA_WEB_ALLOW_UPDATE=1` is now
  set automatically in `cmd_serve()`, so the "Update now" button in the web UI works
  for CLI users — not just the desktop app.
- **Native dialogs for the macOS desktop app.** Port-conflict error and post-update
  restart confirmation now show native `osascript` alerts instead of silently logging
  to stderr (invisible when launched from Finder/Spotlight).
- **AI output toggle persists.** The "Show AI output" preference in the Activity drawer
  is stored in `localStorage` and restored on page reload.

### Fixed

- **Double menu bar icon on macOS.** `app.trayIcon` in `tauri.conf.json` auto-created
  a second tray icon alongside the one created by `TrayIconBuilder::new()` in Rust.
  Removed the config-level entry — only one icon is created now.
- **Window now hides to tray on ✕.** Clicking the window's close button now hides the
  window instead of quitting the app (standard macOS menu-bar behavior). Tray "Quit"
  still exits cleanly.
- **`INDEXA_DESKTOP` and `INDEXA_WEB_ALLOW_UPDATE` not set in the desktop app.** The
  embedded web server never received these env vars, so `POST /api/update/apply` always
  returned 403 and the `relaunch: "desktop"` path was dead. Both are now set before the
  server starts.
- **Update pipeline — three bugs fixed:**
  - `reindexAll()` called `fireJob('deep', …)` (embed only, no summaries). Now calls
    `fireJob('index', …)` (deep + summarize full pipeline).
  - "Generate summary" enqueued items without ever draining the queue (59 rows were
    stuck `pending`). Now calls the draining `fireJob('summarize', path)` path.
  - "Regenerate" was a no-op on already-summarized paths (`enqueue_subtree` uses
    `INSERT OR IGNORE` which cannot reset a `done` row). Added `requeue_subtree` that
    calls `mark_for_resummary` per item, resetting `done`/`failed` → `pending`.
- **HTTP status codes corrected.** `GET /api/summary` when no summary exists: 200 →
  404 (body unchanged for backward compat). `POST /api/models/catalog/refresh` on
  network error: 200 → 502.
- **Watch session memory leak.** Watch tasks that crashed or panicked left zombie
  entries in `state.watch_sessions`, causing the UI to show "watching" indefinitely
  with no events flowing. A watchdog task now removes the session entry on completion.
- **`setModelRole` used blocking native `confirm()`.** Replaced with the existing async
  `confirmModal()` to avoid freezing the browser event loop (which breaks headless and
  automation contexts).
- **`fireJob()` missing error handling.** Did not check `r.ok` before reading
  `d.job_id`; on a 4xx/5xx response this caused a silent runtime error. Now checks
  `r.ok` and shows an error toast on failure.
- **Toolbar Export with no folder selected.** Was opening `/api/export?path=` with an
  empty path. Now shows a "Select a folder first" toast and returns early.
- **Multiple missing `r.ok` checks.** `showSummary`, `setModelRole`, `setProvider`,
  `saveEndpoint`, `saveKey`, `clearKey`, `refreshCatalog` all now check HTTP status
  before attempting to parse the response body, and show error toasts on failure.
- **Treemap cells lacked keyboard focus indicator.** SVG cells have `tabindex=0` but
  no `:focus-visible` style. Added `stroke: var(--accent)` on focus so keyboard users
  can see which cell is focused.
- **README stale version numbers and competitor table removed.** Version pins removed
  (README is now evergreen; version info belongs in CHANGELOG). The "Why it's
  defensible" competitor comparison table replaced with a bold "The only tool of its
  kind" section.
- **Smart classification Undo now actually resets.** The "Undo" button in the Smart
  label chip previously re-posted to `/ignore` (a no-op stub). It now calls
  `POST /api/classifications/reset` which deletes the classification row entirely,
  reverting the path to "no suggestion." Re-running `indexa classify` re-surfaces the
  auto suggestion. Adds `Store::delete_classification` and the `/reset` endpoint.
- **Smart label category dropdown synced to core enum.** The `documents` option was
  offered in the web UI dropdown but has no corresponding `SemanticCategory` variant
  in the core — confirming it persisted an invalid category. Removed from the dropdown;
  valid options are now: code, media, archive, personal, work, system, other.

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

[Unreleased]: https://github.com/harf-promo/indexa/compare/v0.25.1...HEAD
[0.25.1]: https://github.com/harf-promo/indexa/compare/v0.25.0...v0.25.1
[0.25.0]: https://github.com/harf-promo/indexa/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/harf-promo/indexa/compare/v0.23.0...v0.24.0
[0.23.0]: https://github.com/harf-promo/indexa/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/harf-promo/indexa/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/harf-promo/indexa/compare/v0.20.1...v0.21.0
[0.20.1]: https://github.com/harf-promo/indexa/compare/v0.20.0...v0.20.1
[0.20.0]: https://github.com/harf-promo/indexa/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/harf-promo/indexa/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/harf-promo/indexa/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/harf-promo/indexa/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/harf-promo/indexa/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/harf-promo/indexa/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/harf-promo/indexa/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/harf-promo/indexa/compare/v0.12.3...v0.13.0
[0.12.3]: https://github.com/harf-promo/indexa/compare/v0.12.2...v0.12.3
[0.12.2]: https://github.com/harf-promo/indexa/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/harf-promo/indexa/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/harf-promo/indexa/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/harf-promo/indexa/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/harf-promo/indexa/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/harf-promo/indexa/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/harf-promo/indexa/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/harf-promo/indexa/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/harf-promo/indexa/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/harf-promo/indexa/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/harf-promo/indexa/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/harf-promo/indexa/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/harf-promo/indexa/compare/v0.3.5...v0.4.0
[0.3.5]: https://github.com/harf-promo/indexa/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/harf-promo/indexa/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/harf-promo/indexa/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/harf-promo/indexa/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/harf-promo/indexa/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/harf-promo/indexa/compare/v0.2.3...v0.3.0
[0.1.0-rc1]: https://github.com/harf-promo/indexa/releases/tag/v0.1.0-rc1
