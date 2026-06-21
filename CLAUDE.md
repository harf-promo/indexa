# Indexa — Claude Code instructions

## Canonical pitch

Indexa is **the local context engine for AI**. The index is the substrate; context is the product. Never revert to "file indexer" framing in user-facing copy.

Two audiences, one engine: it saves **cloud** AI tools (Claude Code, Cursor, Copilot) their paid token budget, *and* gives **local** models (Ollama, llama.cpp) the context they can't hold in a small window — by serving a retrieved slice instead of the whole repo (separates *working context* from *searchable context*; keeps the KV-cache bounded). Keep the local-model angle as honest as the cloud one — caveats live in `docs/methodology.md`, not the README hero.

**Context Packs** (shipped v0.14) is the term for a subject-scoped bundle: files scattered across the disk that all belong to one topic, grouped into a named context and exported as one portable file (XML/Markdown, never HTML).

The name stays **Indexa** — the AI-"context" namespace is saturated (see `memory/feedback_naming_decision.md`); the tagline, not the name, carries "context."

See `memory/feedback_positioning.md` for full vocabulary guide.

## Local models required

Indexa defaults to local Ollama models. These must be pulled before `indexa deep`/`summarize` will work:

```bash
ollama pull nomic-embed-text   # embedding (~270 MB)
ollama pull gemma3:4b          # file summaries (~2.5 GB)
ollama pull gemma3:12b         # dir roll-ups + Q&A (~8 GB)
```

Verify with `ollama list`.

## Current feature surface (v0.72.0)

**Knowledge-graph depth + multimodal reach (v0.72):** three pending follow-ups, all additive/default-off.
- **Map "Communities" view** (`GET /api/graph?layers=communities`) — the biggest KG UX gap, now done:
  **Louvain** clustering (`Store::detect_communities`, `crates/core/src/store/communities.rs` —
  deterministic, dependency-free; Louvain not LPA so a bridge between two cliques doesn't collapse into
  one "monster community") tints nodes by community (≤6 low-sat HSL tints, SVG data layer only — the one
  sanctioned categorical-colour exception), surfaces each community's **hub** (highest-PageRank file), and
  marks cross-community **bridge** edges ("surprising connections"). Computed over the structural call
  graph (overlay edges don't shift membership), inline + fail-open; "approximate" caveat in the legend.
- **"Same pack" graph layer** (`?layers=pack`) — completes the Map overlay set (semantic + category +
  pack), star-per-pack (`Store::pack_file_edges`), exact (user curation). Combine: `?layers=semantic,category,pack,communities`.
- **`indexa multimodal [--enable]`** — readiness report (detects tesseract/pdftoppm, ffmpeg, whisper-cli +
  an Ollama vision model) + safe one-command enable of the `[parsers.*]` flags via `config::load`→`save`
  (refuses to clobber an unparseable config; honors `--config`). Same report is a new `indexa doctor` section.

**Brand identity + knowledge-graph depth (v0.71):**

**Brand identity + knowledge-graph depth (v0.71):**
- **First real logo + app icon** (was a placeholder). The mark is the design system's own signature —
  the **green Harf apostrophe `#A4CD39` on an ink ground** — authored once as `crates/web/assets/ui/
  favicon.svg` and used everywhere: browser favicon (`GET /favicon.svg`, was absent), the full desktop
  app-icon set (regenerated via `tauri icon` → `icon.icns`/`.ico`/PNGs/Store logos, so the installed
  macOS app/Dock/tray show it; `tauri.conf.json bundle.icon` already referenced them, tray reuses
  `default_window_icon`), the web header (apostrophe mark + grey "indexa" wordmark, replacing the
  generic `⊡`), and a README lockup (`docs/assets/logo.svg`).
- **"Same category" knowledge-graph layer** (Track 3 follow-up, `GET /api/graph?layers=category`,
  default-off) — files sharing a confirmed classification category, grouped via a deterministic star
  per category (O(n), `Store::category_file_edges`, fresh-conn, fail-open, no schema). Joins the v0.70
  "Related by meaning" semantic layer (`?layers=semantic,category`); dashed-grey grouping edges; no
  `layers` ⇒ byte-identical. (Pack-super-node layer + one-command multimodal-enable remain follow-ups.)

**Local-context engine, sharpened (v0.70):** a four-feature release driven by the owner's questions
about how the MCP serves other AI sessions, plus the remaining roadmap tracks (full detail in
`CHANGELOG.md`):
- **MCP/CLI/web "retrieval-only" Ask + model transparency** — when another tool calls the Indexa MCP
  `ask`, the answer is synthesized by Indexa's **local** model (e.g. `ollama/gemma3:12b`), never the
  caller's; most other tools are pure retrieval. New `synthesize:false` (MCP) / `--no-synthesize`
  (CLI) / `"synthesize":false` (web) returns the **packed context slice** (full pipeline: hybrid +
  boosts + rerank + MMR + per-file cap + overview + coverage) for a stronger caller to answer with its
  own model; synthesized answers report which local model produced them (`Answer.model`). Optional
  param ⇒ MCP **tool count stays 46**; single-shot path byte-identical.
- **Multimodal verified end-to-end** — image captioning, PDF OCR, audio transcription, video frames
  were wired but only error-tested; added `#[ignore]`-gated happy-path e2e (`crates/parsers/tests/
  multimodal_live.rs`, `crates/llm/tests/caption_live.rs`) on committed `fixtures/multimodal/`, all
  four verified live. Default caption model `gemma3:4b` works on a stock Ollama; some builds reject
  `llama3.2-vision` (`mllama` arch) — use `moondream`/the default.
- **GraphRAG "Approach C"** (`[retrieval] graphrag_clusters`, default-off) — groups a broad answer's
  hits into `=== THEME … ===` clusters (greedy cosine over MMR's embeddings; optional per-cluster LLM
  theme via `graphrag_summarize`). Off path byte-identical; single global `[1..N]` citations; fails
  open. Live A/B on Indexa's cohesive corpus ≈ flat (collapses to ~1 cluster) → ships unpromoted, a
  lever for topic-diverse corpora.
- **Knowledge-graph upgrade** (Track 3, `GET /api/graph?layers=semantic`, default-off) — overlays
  meaning-similarity edges (per-file centroid → cosine ≥ 0.78, `Store::semantic_file_edges`, fresh
  conn, fail-open, no schema) on the Map's call graph with a "Related by meaning" toggle. No `layers`
  ⇒ byte-identical.

**Hardening, perf & batching (v0.69):** a fixes + dedup + perf release bundling the post-0.68
`[Unreleased]` work, much of it from a 12-agent Workflow audit (full detail in `CHANGELOG.md`):
- **Added:** per-session savings ledger (`GET /api/session-impact/{id}`; migration-guarded
  `session_id` on `tool_usage`) · **C and C++ in the code graph** (8 languages; +tree-sitter-c/-cpp,
  openssl-free preserved) · **batched cloud embedding** (OpenAI array-`input` / Google
  `:batchEmbedContents`) that **fails open** to sequential on any error/count/dim/index mismatch —
  a batch can never misalign or lose a file's vectors (live speedup is `#[ignore]`-test-gated).
- **Fixed:** `POST /api/keys` no longer wipes the config (+ stored keys) when `config.toml` fails to
  parse · both `text.rs` chunker stride loops `.max(1)`-guarded against an infinite loop on a
  degenerate `size`/`overlap` config · secret redaction preserves the original `:`/`=` separator so
  a redacted YAML/TOML config stays valid.
- **Dedup/perf (internal, behavior-neutral):** `indexa_core::pathutil` (`path_depth`/
  `ancestor_dirs_to_root`) + `store::chunk_content_hash`; `AnswerImpact: Serialize` (one wire shape,
  drops the per-surface DTOs); `helpers::now_unix`/`expand`/`base_name`; `jobs_exec::throughput_eta`;
  `paths_for_ids` + `reconcile_entries` batched SQL.
- **Docs trued up:** USAGE.md (MCP **46** tools, real `[retrieval]` defaults), `config.md`,
  `methodology.md`. ⚠️ Audit findings need re-verification — ~10% were false positives/traps (caught
  before merge; see `MEMORY.md` / `memory/project_lore.md`). NO new runtime deps; openssl-free.

**Trust, design & retrieval (v0.68):** the eval-instrumentation + Harf-restyle release (full detail
in `CHANGELOG.md`):
- **`indexa deep --no-embed`** — FTS-only hermetic index (no Ollama, no embeddings; a later plain
  `deep` self-heals the vectors via the `COUNT(*)=COUNT(embedding)` skip check). Powers a new
  advisory CI job that runs `indexa eval` on Indexa's own `fixtures/self-golden.json` every PR.
- **`indexa eval` gained recall@k + nDCG@k** and a **baseline regression gate** (`--baseline` /
  `--max-regression`, epsilon-guarded) — retrieval changes are now eval-gated.
- Answer **"confidence" → "retrieval coverage"** across CLI/web/MCP/docs (display-only; the
  `--json`/MCP/SSE field stays named `confidence`).
- **Web UI restyled onto the Harf design system** (`crates/web/assets/ui`): grey + green-as-
  punctuation palette, **teal** active-states (green is never UI state), Geist fonts, sharp corners
  + hairlines, ink primary buttons, `light-dark()` dark mode, no emoji, a footer "by Harf" mark.
  `01-tokens.css` is now the Harf foundation (legacy `--bg`/`--surface`/`--accent`/… alias onto it);
  brand source = the "Harf Design System" project via the `claude_design` MCP (`DesignSync` tool).
- **Sparse/keyword search tokenizes the query** (`store/search.rs::build_fts_query`) instead of
  phrase-matching it whole — `"phrase" OR "term1" OR "term2" …` (stopwords dropped, BM25 ranks), so
  multi-word natural-language questions actually match in `--mode sparse` (self-golden hit-rate
  0.69→1.00). Also feeds the lexical arm of hybrid `rrf`. (Track 2 retrieval intelligence, PR #1.)

**Hardening, parity & performance (v0.67):** a defect/parity/perf release from three adversarial
review sweeps — no single headline feature. **Security:** web `GET /api/packs/{name}/export` now
runs `redact_secrets` (it was the one export surface that didn't). **Correctness:** cite budget-
truncated chunks (no dangling `[N]`); `parse_reindex_interval` is char-boundary-safe (was a
multibyte panic reachable via `--changed-since` / `?changed_since=` / `[scan] auto_reindex`);
`indexa watch` now writes the `entries` row for newly-created files (were never summarized + pruned)
AND recomputes the path hint on every upsert via `surface::classify` (a watch edit used to NULL
`hint_cat`/`deep_policy` — that fix landed as a same-session regression catch); `cmd_update` clears
the CLI-skew marker so the web banner unsticks; fingerprint `**` markers are rejected (not silently
single-`*`). **Parity (MCP tool count stays 46 — optional params only):** `search`/`search_pack`
emit chunk `#seq`; `export_pack` gained `changed_since`/`category`; `code_graph` gained `cycles`;
`ask` shows per-answer impact + accepts `top_k`; `read_file` accepts a byte `offset` (paging past
the 40 KB cap); web Insights gained `largest` + `languages`. **Config:** new `[retrieval]
archive_segments` + `archive_penalty` (extend/disable the historical down-weighting; `0.0` disables).
**Perf:** `tree_level` replaced ~4×C correlated subtree-LIKE subqueries with one set-based
aggregation pass (proven behavior-identical by a `tree_level_reference` equivalence oracle test), and
web `api_tree` reads on a fresh connection instead of holding the shared store mutex. **Redundancy:**
XML escaping + `floor_char_boundary` consolidated into `indexa_core::text`. **Deps:** zip 8, kamadak-
exif 0.6, notify 8 + debouncer 0.7, axum 0.8, setup-node 6 (openssl-free preserved). ⚠️ Process
lessons this cycle → [[project-lore]] (Windows crates.io download flake = rerun, not a code bug;
`gh run watch --exit-status` unreliable — confirm with `gh pr checks`; equivalence-oracle for risky
SQL rewrites) + [[feedback-commit-signing]] (`--no-gpg-sign` on ALL commit-creating git ops).

**Application & structure recognition (v0.66):** Indexa now understands *groups* of files in a
recognizable layout, not just individual files — that a directory is a Rust crate, a Next.js app, a
Django project, a macOS `.app` bundle, a Terraform module, a Jupyter project, etc. **Grammar** (in
`crates/core/src/fingerprint.rs`): `FingerprintDef` extended (backward-compatibly, all new fields
`#[serde(default)]`) with `any_of`/`none_of` (anti-markers), `kind`/`family`(code|os|infra|data)/
`specificity` (most-specific-wins), `provenance`. Markers parse by shape: `Cargo.toml`=DirectChild,
`Contents/Info.plist`=RelPath (nested, tested against the full entry set), `*.xcodeproj`=ChildGlob
(tiny hand-written `*`/`?` matcher — do NOT promote `globset`; `**` rejected). `detect()` stays pure
(builds `children` map + `all_paths` set). **Persistence** (re-derivable, follows the
`classifications` lifecycle NOT decisions/weights): new `directory_apps` table
(`store/schema.rs` base DDL; multiple rows/dir, `is_primary` = specificity winner) + `store/dir_apps.rs`
(`DetectedApp` + `replace_apps_for_dir`/`apps_for_dir`/`primary_app_for_dir`/`all_detected_apps`/
`primary_apps_under`); cleared by all three `entries.rs` delete paths + the `prune.rs` orphan sweep
(both **orphan guard tests updated** — `directory_apps` added to `orphan_rows_for`/`seed_full_entry`).
**Detection pass** = a SIBLING of `run_detectors` (NOT folded in — that carries `ReviewConfig`/fatigue
caps): `crates/core/src/app_detect.rs::detect_directory_apps(store, &defs)` runs `fingerprint::detect`
over `all_entry_paths`, inverts to per-dir winners (dedup by kind), rewrites rows; wired into
`apps/indexa/src/commands/index.rs` `detector_pass` after `run_detectors`, **fail-open**. **Surfaced**
(extend-only, **MCP tool count stays 46**): project overview annotation (`qa/retrieve.rs
build_project_overview` — one `primary_apps_under` query, budget-safe → `ask` broad answers know the
stack), `indexa inspect` "App" line, web `/api/inspect` `apps[]` → `05-summary.js` "App" row, MCP
`inspect` `app:` line (`project_overview` gets it free), and `indexa fingerprint` now reads the
persisted table (live-compute fallback when empty, e.g. scan-only). **Library** = curated
`fingerprints_builtin.json` (4 families) + `fingerprints_seed.json` seeded OFFLINE from CycloneDX
cdxgen project-types (Apache-2.0, per-rule provenance) via `tools/gen-fingerprints` (excluded from the
workspace, maintainer-run; runtime NEVER fetches) + user `fingerprints.json`. NO new runtime deps;
openssl-free; fail-open.

**Version sync — no more skew (v0.65):** fixes the class of bug where the desktop app updates but the
standalone CLI it spawns (and the MCP server behind `indexa mcp`) silently stays several versions
behind, serving stale behavior with no signal. New shared **skew detector** in `crates/update/src/skew.rs`
(`Skew {InSync|CliBehind|CliAhead|Unknown}` + `Surface {Cli|Mcp}` + pure `classify_skew` + a no-dep
`parse_plist_short_version` that anchors on the exact `<key>CFBundleShortVersionString</key>` element —
NOT a loose "Version" substring, which would grab `CFBundleInfoDictionaryVersion`'s `6.0` — +
`installed_app_version()` reading `/Applications/Indexa.app/Contents/Info.plist`, macOS-only/`None`
elsewhere + `detect_skew` + `Skew::advice(Surface)` single-source-of-truth message). Surfaced in
**`indexa doctor`** ("Version sync" section: ✅/ℹ️/⚠️), **`indexa status`** (`app_version`/`version_skew`
JSON fields via the pure `skew_fields` helper + a human line), and **MCP `get_stats`** (warns an agent it's
on a stale binary; **tool count stays 46**). The desktop's post-update CLI auto-refresh (silent best-effort
since v0.39 — the actual root cause) now **verifies** the installed binary's `--version` and writes/clears
`<data_dir>/cli_skew_warning.json` (`CLI_SKEW_MARKER_FILE`, shared const); web `GET /api/health` reads it
(`read_cli_skew_marker`, pure+tested) into a `cli_skew` field → a second dismissible banner in `27-health.js`
(edited in place, concat unchanged). `download_cli_to`'s macOS codesign failure is now logged, not swallowed.
⚠️ doctor/status/MCP are the **authoritative** detectors (they run in the user's real shell / as the running
binary); the desktop marker + web banner are **secondary** (the app's `resolve_cli_dir()` walks the launchd
`$PATH`, which can resolve a different `indexa` than the user's terminal). All fail-open; NO new third-party
deps (only an internal `indexa-update` path-dep into `crates/mcp`); openssl-free preserved. ⚠️ The "only the
newest changelog shows when updating across versions" report was NOT a bug — cumulative changelog shipped in
**v0.52** (`indexa_update::cumulative_notes`); a pre-0.52 binary running the update just predates it.

**Conversational & complete (v0.64):** three features in one release. **(1) Multi-turn / Conversational
Ask** — schema-backed sessions (`ask_sessions` + `conversation_turns` in `crates/core/src/store/schema.rs`;
store methods in `store/sessions.rs`). The qa crate takes history as a `&[PriorTurn]` value arg (stays
schema-agnostic; `&Store` never crosses `.await`): new `answer_with_ann_history` / `answer_stream_with_ann_history`
/ `answer_agentic_history` / `answer_agentic_stream_history` in `crates/query/src/qa/{synthesize,agentic}.rs`,
which the existing single-shot fns delegate to with `&[]` (byte-identical, `Answer` struct unchanged). A
**follow-up rewrite** (`qa/rewrite.rs::resolve_search_query`) turns "and why?" into a standalone query —
**one extra `llm.generate()` only when history is non-empty**, fail-open. `build_prompt` gains a budget-clamped
`CONVERSATION SO FAR` block (≤25% of `context_budget`, oldest-first trim via `split_history_budget`/
`render_history_block`); `trim_continuation` is kept (multi-turn makes a hallucinated trailing `QUESTION:`
MORE likely). Threaded through web (`AskRequest.session_id`, both handlers + SSE `done` echo, best-effort
`append_turn`), MCP (`AskParams.session_id` — **tool count stays 46**), CLI (`--session-id`/`--continue` +
`default_data_dir()/last_ask_session` pointer file), and the web chat (`06-chat-settings.js` client UUID +
"＋ New" reset). **(2) MCP Resources + Prompts** — server was tools-only; now `enable_resources()` +
`enable_prompts()` with hand-written `ServerHandler` methods (no router macro for resources; avoids
macro-stacking risk) delegating to inner methods in `crates/mcp/src/{resources,prompts}.rs`. **4 resources**
(`indexa://overview · packs · pack/{name} · summary/{path}`, secrets redacted via the shared
`packs::export_pack_body`) + **3 prompts** (`onboarding-overview · explain-file · pack-context`); golden
list in `golden_prompts.txt`. **(3) Markdown tables** in `renderMarkdown` (`08-util-palette-init.js`) +
a gentle "use a table when comparing" prompt nudge. **Feature-completeness:** `confidence.uncovered` now
populated (salient question terms absent from every cited source; `compute_uncovered` in `qa/confidence.rs`,
surfaced in CLI/MCP/web) — no longer a permanent `None`; **presentation speaker-note↔slide mapping** now
follows the rels graph (`ppt/slides/_rels/slideN.xml.rels`) not ordinal position (fixes the sparse-notes
off-by-one). 642 workspace tests. ⚠️ No new deps; openssl-free; single-shot Ask path unchanged (zero added
latency); `19-conversation.css` joined the `include_str!` concat.

**CLI commands** (`indexa <cmd>`): `index` (one-shot scan→deep→summarize; `--contextual` flag) · `scan` · `deep` (`--contextual` flag) ·
`summarize` · `describe` (no-path = whole-project overview; with path = per-file summary) · `inspect` (per-path "what's indexed here") · `map` · `worker` · `pack`
(Context Packs; `pack add-url` = remote sources) · `weight` (Importance
weighting) · `insights` (duplicates/stale/diff) · `graph` (file-to-file call graph) · `export` ·
`ask` · `watch` · `serve` (`--host 0.0.0.0` for LAN) · `mcp` (+ `mcp install [--client]`, auto-detects)
· `completion <shell>` · `status` (`--json` incl. per-tool savings) · `rm` · `prune` (orphan-row GC) ·
`doctor` (`--apply-ollama-env`) · `fingerprint` · `classify` · `update`.

**Sharper retrieval (v0.43):** **Candle cross-encoder reranker** — `CandleReranker` in `crates/query/src/rerank.rs` uses `DebertaV2SeqClassificationModel` from `candle-transformers` with `mixedbread-ai/mxbai-rerank-xsmall-v1` (~85 MB, Apache-2.0, CPU-only); model downloaded via `hf-hub` (online feature, sync API) on first use, cached at `~/.cache/huggingface/hub/`; initialized once per process via `static CANDLE_INNER: OnceLock`; scores each (query, doc) pair in a `spawn_blocking` task; returns sorted indices; falls open on load/score failure. `[retrieval] rerank_backend = "llm" | "cross-encoder"` config field added to `RetrievalConfig` and `QaConfig`. **MCP `ask` tool** now accepts `rerank: Option<bool>` + `rerank_backend: Option<String>` params — agents can enable reranking per call. ⚠️ `onig` in tokenizers links statically (bundled C source via `cc::Build`), same pattern as rusqlite's bundled SQLite — safe for macOS notarization. ⚠️ **`hf-hub` MUST use `0.5` with `default-features = false, features = ["ureq"]`** → ureq 3's default rustls+ring TLS, NOT native-tls/`default-tls`/openssl. The v0.43.0 tag shipped hf-hub 0.3 `online` (ureq+native-tls), which pulled `openssl-sys` and FAILED the `aarch64-unknown-linux-gnu` cross-compile (macOS local builds passed because native-tls uses Security.framework there — openssl only enters on Linux). v0.43.1 fixed it; keeps the tree openssl-free, consistent with the reqwest/rustls invariant. Verify any hf-hub change with `cargo tree -i openssl-sys --target aarch64-unknown-linux-gnu` (must be empty).

**Fast, legible & visible (v0.42):** (A) **Embedding cache** — `content_hash TEXT` column on `chunks`; SHA-256 of raw chunk text used as cache key; re-indexing skips embedding unchanged chunks (`cached_embeddings_by_hash` store method; schema migration in `schema.rs` with IMMEDIATE-transaction guard for concurrency). (B) **MMR diversity** — `apply_mmr` + `mmr_score` + `cosine` in `qa.rs`; `embeddings_for_chunks` store method; wired into `retrieve()` after boosts; `[retrieval] mmr_lambda` config (default 0.5; 1.0 = off; fails open). (C) **45 MCP tools** (was 42): `project_overview` (calls `build_project_overview` + `is_broad_intent`, now pub), `explain_retrieval` (calls `explain_retrieval` from qa.rs), `inspect` (calls same store methods as web inspect handler); `golden_tools.txt` updated; doc counts updated. (D) **`indexa describe` no-path** → whole-project overview (`describe.rs`; `path: Option<String>` in CLI). (E) **Auto-preflight** — `preflight_ollama(cfg)` in `helpers.rs` (liveness + model-presence probe extracted from doctor.rs); called at top of `cmd_index` + `cmd_deep`. (F) **UX polish** — Ask welcome gets project-level example chip; "Why these sources?" gets caption; graph centrality tooltip glossed; health banner "Re-index now" button; CLI hints → `indexa index <path>`. (G) **Simplification** — 4 JS escape clones deleted (→ `escapeHtml`); `ollama.rs` extracted `build_describe_prompt`/`build_dir_prompt`; web contextual loop uses `contextual_embed_texts` helper (kills loop drift); dead `micro_benchmark` config field removed; `indexa_core::text::{truncate_chars, snippet}` shared util. ⚠️ DO NOT reintroduce the `total−used` basis (v0.28.1 invariant) or the `micro_benchmark` field.

**Memory budget invariant (v0.28.1):** `resource::compute_budget` keys on `available_bytes`
(sysinfo `available_memory()` = macOS XNU active+inactive+free), NOT `total − used_memory()` —
sysinfo 0.39's `used_memory()` includes the macOS **compressor**, so `total−used` falsely refuses
models. Don't reintroduce the `total−used` basis. The web Impact dashboard (`/api/impact`),
responsive layout (≤1024px drawer / ≤768px stack), and arrow-key tree a11y also shipped in v0.28.

**Answer-quality invariant (v0.29):** retrieval auto-**down-weights** historical paths — `retrieve()`
in `crates/query/src/qa.rs` calls `apply_archive_penalty` (×`ARCHIVE_PENALTY` 0.15) on hits whose path
has a segment in `HISTORICAL_SEGMENTS` (`archive`/`archived`/`historical`/`deprecated`/`old`), skipped
when the question is explicitly scoped *into* such a path, then re-sorts. `build_prompt` instructs
"answer only the question, prefer current over archived"; `synthesize_from_hits` runs `trim_continuation`
to cut any hallucinated `QUESTION:`/`ANSWER:` second turn. Don't remove these — they fix answers citing
`docs/archive/` + claiming unshipped versions. v0.29 web/desktop: Map self-refreshes on job-done
(`refreshMap` in `07-map.js`, fired from the SSE `done` handler) + a "What is this?" plain-language
explainer; drag-resizable sidebar (`23-sidebar-resize.js`, persists `--sidebar-width`) with
hover-revealed row actions so folder names aren't clipped; the desktop "Check for Updates" shows
version + CHANGELOG notes (release.yml feeds `latest.json` `notes` via tauri-action `releaseBody`) then
restarts, and a new app/tray "Install command-line tool" item runs `indexa_update::download_cli_to`
(non-self-replace CLI download → a PATH dir).

**Understand the whole project (v0.41):** (A) **Presentation parsing** — `crates/parsers/src/presentation.rs` (`PresentationParser`); slides extracted from OOXML zip (`ppt/slides/slideN.xml`), numerically sorted, speaker notes from `ppt/notesSlides/notesSlideN.xml`, one chunk per slide. `.ppt` (OLE binary) quiet stub in `office.rs`. Richer `.docx` reads headers/footers/footnotes/endnotes. (B) **Whole-project synthesis** — `is_broad_intent(q)` detector + `build_project_overview(store, hits, scope, budget)` in `qa.rs`; broad questions inject a PROJECT OVERVIEW block (root roll-up + child one-liners) into `pack_context` before chunk citations; `retrieve_and_rerank` + `agentic_retrieve` both return `(hits, overview)`. Budget: broad → `context_budget*35%`, specific → 300 chars. `synthesize_from_hits` + `build_prompt` updated with overview guidance. (C) **Contextual Retrieval** — `crates/query/src/contextual.rs` shared helper (`build_doc_context`, `contextual_embed_texts`, `build_blurb_prompt`); `--contextual` flag on `deep`/`index`; wired in `deep.rs` using the helper; `jobs_exec.rs` refactored to call `build_doc_context`/`build_blurb_prompt` from the shared helper (kills prompt drift). 549 tests. ⚠️ Known gaps: scanned-PDF OCR, Apple iWork, chart/SmartArt text — documented in `docs/methodology.md`.

**Readable & quiet (v0.40):** (A) **Changelog markdown rendering** — the in-app update window (`showUpdateChangelog`
in `15-update.js`) now calls `renderMarkdown(reflowChangelog(notes))` instead of `notesEl.textContent = raw`.
`reflowChangelog` merges hard-wrapped CHANGELOG continuation lines so `renderMarkdown`'s line-based parser
doesn't close list items prematurely. Light-theme CSS overrides in `16-update-changelog.css` (white card
background → dark headings, gray code chips). (B) **Near-dup basename filter** — `near_dup_same_basenames`
(new helper in `detectors.rs`) gates the seeding loop and `sweep_filtered_noise`: a near-dup cluster
(similarity-based, not byte-identical) now only opens a "which is canonical?" question when all members share
the same filename. Differently-named files that happen to be topically similar no longer flood the inbox.
Exact-content clusters always ask regardless of name. 5 new tests; 520 total. ⚠️ The MCP server must be
restarted after each CLI update so it spawns the new binary — MCP version skew remains device-only/user-triggered.

**Trustworthy & current (v0.39):** fixes the owner's "stale binary / noisy review / wrong answers" audit.
**(A) Review noise** — `crates/core/src/decisions/detectors.rs`: duplicate decisions skip non-actionable
clusters (`duplicate_cluster_actionable`: all-asset extensions `DUP_SKIP_EXTS` or any member in
`DUP_SKIP_DIR_FRAGMENTS` generated/vendored trees); `symbol_ambiguity` is OFF by default
(`ReviewConfig.symbol_ambiguity`, `[review] symbol_ambiguity`) + an idiom denylist (`is_idiom_symbol`:
`new`/`default`/`parse`/… + `with_`/`set_`/`get_` prefixes) + `SYMBOL_AMBIGUITY_MAX_DEFINERS` ceiling;
`sweep_filtered_noise` retroactively dismisses existing noise (run from `run_detectors` + `indexa prune`,
respects the config flag). **(B) Code answers** — `qa.rs apply_code_intent_boost` (×1.6 code-file hits on
code-intent questions: `is_code_intent` terms / snake_case; always-on like `apply_archive_penalty`, inert
on prose/non-code) fixes the doc-bias where "which function…" returned only docs. **(C) Visibility** —
`get_stats` (MCP) shows server version + index-age staleness; `GET /api/health` (`handlers/health.rs`,
`STALE_AFTER_DAYS=7`) + `27-health.js` banner; desktop `install_update` refreshes the CLI in place via
`download_cli_to` (the version-skew root-cause fix). ⚠️ The 3 "bugs" an adversarial agent flagged
(`trim_continuation` slice, `delete_subtree` prefix, redact count) were VERIFIED false positives — don't
"fix" them. Desktop background auto-watch wiring + `doctor` skew are deferred to v0.40 (device-only verify).

**Multimodal memory-safety (v0.38, "safe"):** the watchdog now counts vision/caption models. New
vision footprints in `resource.rs` `MODEL_FOOTPRINTS` (`llama3.2-vision`, `:11b` alias, `moondream`;
conservative Q4 estimates, NOT measured). `resident_peak_set` (N-model dedup peak) + `caption_fit_report`
→ `CaptionFit {caption_model, caption_peak_bytes, trio_peak_bytes, budget_bytes, fits, caption_model_known,
lighter_suggestion}` — the {file,dir,caption} co-resident trio vs `compute_budget`, suggests a lighter
known vision model when it overflows. Surfaced via `POST /api/config/features` (`handlers/config.rs`
`api_config_features_set` gained `State`; `caption_budget_warning` helper) → JSON `caption_warning`
(honest, NON-blocking — local-first); `07-map.js saveFeatures` toasts it. `index.html` "Not yet counted"
notes replaced. **Audio transcribe EXCLUDED** (external `whisper-cli`, not Ollama). Don't reintroduce the
`compute_budget` `total−used` basis (v0.28.1 invariant). 4 `resource.rs` unit tests + live e2e.

**Durable view: deep-linking + window-state (v0.37):** (A) **Deep-linkable URL state** — `26-url-state.js`
mirrors the active tab + selected path + last Ask question into `location.hash` (`#tab=…&path=…&q=…&scope=file`;
`tree` omitted as default) via `writeHash` (`history.replaceState`, guarded by `__suppressHashWrite`) and
restores on load via the hoisted `restoreFromHash` (⚠️ call the BARE hoisted name from `08`'s boot, NOT
`window.__indexaRestoreHash` — the window assignment runs later, in `26`). Hooks: `switchTab` (01),
`showSummary` (05, also sets `selectedPath`), `doAsk` (06), boot (08, replaces unconditional
`switchTab('tree')` + sets `window.__indexaHashRestored`), onboarding guard (11). Restore fires ONE
`/api/summary`, ZERO `/api/ask` (question is display-only, never auto-run); `hashchange` re-restores on
Back/Forward. (B) **Desktop window-state** — `tauri-plugin-window-state` v2 (`.plugin(...Builder::default().build())`
in `main.rs`, dep in desktop `Cargo.toml`; MIT/Apache, cargo-deny clean) remembers size/position;
tauri.conf width/height are first-launch defaults, minWidth/minHeight clamp. **v0.38 "safe" (multimodal
memory budget) is split out** per the blueprint (touches the `compute_budget` honesty invariant).

**Navigable knowledge graph (v0.36, "see the graph"):** the Map's force-directed call graph
(`19-graph.js`, `#map-panel-graph`) is now interactive — extended in place, NOT a new sub-view.
Click/Enter a node → `focusNode` locks a persistent highlight (`graphState.setHighlight`/`lockedId`,
published from `renderGraph`'s closure) + shows `#graph-focus-bar`; **Expand neighbors**
(`expandFocusNeighbors`) re-fetches `GET /api/graph?focus=<path>&depth=1|2` — a new read-only
neighborhood filter (`apply_focus` in `handlers/graph.rs`, pure in-memory BFS over the already-scoped
graph, `edge_tiers` filtered in lockstep, NO schema/Store change); **Show all in scope**
(`resetGraphView`) clears it. Nodes sized by PageRank (unchanged `r` formula), edges styled by tier
(`tier-import` accent / `tier-bare` dashed-muted). New `25-graph-explore.js` (legend + plain-language
"What is this?" help + focus/expand/reset; reuses `escG`/`graphState`/`fetchGraph`/`currentGraphScope`
from `19-graph.js`, concatenated before it) + `css/18-graph-explore.css` (re-asserts `.graph-edge.hl/.dim`
LAST so dimming wins over per-tier opacity). Legend swatches are text-labelled (aria, not color-only);
bare-name caveat shown only when `bare_edges>0`/`strict`. Both files wired into the `lib.rs` concat.

**Legible retrieval (v0.35):** two read-only web surfaces of existing CLI capability. `POST
/api/ask/explain` (`handlers/ask.rs` `api_ask_explain`) calls `indexa_query::explain_retrieval` → JSON
`{mode,top_k,…,stages:[{label,hits:[{rank,path,heading,score}]}]}` (the `ask --explain` trace); the Ask
answer gets a "Why these sources?" `<details>` (`06-chat-settings.js` `renderExplainTrace`/`loadExplain`,
delegated click). `GET /api/inspect?path=` (`handlers/inspect.rs`, reuses `Store::entry_by_path`/
`chunks_for_path`/`classification_for`/`weight_for`/`edges_from`) → an "Indexed facts" `<details>` under
the summary (`05-summary.js` `appendInspectFacts`). Styles in `css/17-legible.css`. No answer-pipeline
change (SourceCitation untouched) — explain is a separate on-demand pass.

**Updater window + file preview (v0.34):** the desktop "Check for Updates" is now a fully **in-app**
flow — no osascript dialog. `install_update` (`apps/indexa-desktop/src/main.rs`) publishes
`UpdateProgress::available(version, body)` (full changelog) → the webview shows `#update-changelog-modal`
(white scrollable card, Install/Later; `15-update.js` + `css/16-update-changelog.css`) → the user's
choice flows web→Rust via `POST /api/update/control` → a process-global `watch<Option<UpdateCommand>>`
in `crates/web/src/update_control.rs` (`wait_for_command`, ⚠️ copy the value out before `send(None)` or
it self-deadlocks) → existing download/progress overlay → restart. `AtomicBool INSTALL_IN_PROGRESS`
guards tray+menu double-fire; control endpoint is `INDEXA_DESKTOP`-gated (403 under plain serve).
**File preview:** `GET /api/file?path=` (`handlers/file_preview.rs`; path-within-roots like MCP
read_file, 40 KB cap, NUL→binary) → `#file-preview-pane` split beside `#summary-view` in `.context-split`
(`24-file-preview.js` + `css/15-file-preview.css`), driven from `05-summary.js` on file select.
**Highlighting is a self-written client tokenizer** (keyword/string/comment/number → `.hl-*` tokens) —
NOT tree-sitter-highlight, which can't pair with the tree-sitter 0.26 the parsers use (no 0.26-compatible
release). Keep it client-side + dependency-free.

**Update-progress invariant (v0.30):** the desktop self-update + CLI install show a **live progress
bar** bridged Rust→web **without Tauri IPC** (the webview loads a remote URL — no `withGlobalTauri`).
A process-global `watch` channel in `crates/web/src/update_progress.rs` (`report_update_progress` /
`UpdateProgress`) is streamed over `GET /api/update/progress/stream` (SSE, mirrors
`handlers/telemetry.rs`); `15-update.js` renders `#update-overlay` (reusing `.engine-job-bar`/
`.engine-job-fill`). The desktop's `install_update`/`run_cli_install` (`apps/indexa-desktop/src/main.rs`)
publish phases (downloading→installing→done/error); `download_cli_to` streams via `Response::chunk`
(no reqwest `stream` feature) + an injected `on_progress` callback — so `crates/update` stays
web-agnostic (no circular dep). Channel stays `idle` under plain `indexa serve`, so the overlay never
shows there.

**Export invariants (v0.31, "exports that fit"):** export is summary-tree based (`crates/query/src/
export.rs`); v0.31 adds (1) `--signatures` — a code-skeleton render (`render_signatures` + heuristic
`extract_signature`; reads `Store::code_chunks_under`, language-tagged chunks, NOT summaries — works
after `deep`); (2) `--token-budget N` (+ `--strict-budget` to fail) via `approx_tokens`; (3)
**secret-scan-on-export** — `crates/query/src/redact.rs` `redact_secrets` runs on ALL export surfaces
(CLI `export`/`pack export`, MCP `export_pack`, web `api_export`) before content leaves the machine,
opt-out `--no-redact`; (4) `--clipboard` (native `pbcopy`/`clip`/`wl-copy`/`xclip`, NO arboard dep —
keeps Linux CI X11-free) + `--strip-comments`. Shared `finalize_export`/`ExportSink` in
`apps/indexa/src/commands/helpers.rs` (redact→budget→clipboard/file/stdout). **Recency boost (opt-in):**
`[retrieval] recency_boost`/`recency_days` → `Store::boost_with_recency` in `qa.rs retrieve()` after
`apply_archive_penalty` (positive twin; mtime-based, NOT git). Don't add a whole-repo "dump" mode — the
retrieved-slice model is the moat (vs repomix/gitingest token bricks). **v0.58 ("sliced exports"):**
relational slicing on CLI `export` — `--changed-since <7d|12h|90m|3600s>` (reuses
`parse_reindex_interval` + new `Store::paths_modified_since`) and `--category <cat>` (reuses
`Store::classifications_in_category`); both build an allow-set (`build_export_filter`, intersect when
both given) and prune the built tree via the pure `prune_tree` in `export.rs` (file kept iff in the set;
dir kept iff a descendant is — applied AFTER `build_tree`, render/redact/budget untouched). **Export
honesty:** `cmd_export` now `bail!`s (stderr + non-zero exit) on no-index / no-summaries / empty-output
instead of `println!`-to-stdout + `Ok(())` — a silent success used to write the notice INTO a piped
file. **v0.60 ("sliced exports everywhere"):** `build_export_filter` was LIFTED into
`indexa_query` (pub, single source of truth) and the `--changed-since`/`--category` slice now
reaches ALL FOUR export surfaces — CLI `export`, CLI `pack export`, web `GET /api/export`, web
`GET /api/packs/:name/export` (filters as query params) — plus two optional "changed since" /
"category" fields in the web Export menu (`05-summary.js doExport`). Empty slice → loud failure
everywhere (CLI bail / web 404|422), never a silent empty artifact.

**Per-Ask impact readout (v0.59, "see the savings"):** every `ask` surfaces the concrete
"retrieve the slice" win for that answer. `crates/query/src/impact.rs` — `AnswerImpact
{served_bytes, counterfactual_bytes}` + `saved_percent()` (capped at 99 — a real answer always
serves something, so never "100% less") + `is_meaningful()` (gates the readout: cited files
existed AND serving was smaller) + `human()`; `served_bytes(answer)` = answer text + delivered
citations (shared accounting across surfaces). Byte formatting unified in
`indexa_core::text::human_bytes` (usage.rs `human_size` now delegates to it). Surfaced: web
stream terminal `done` event gains an `impact` object → `06-chat-settings.js renderImpact()`
under the answer; buffered `/api/ask` → `AskResponse.impact`; CLI `ask` prints an `impact:` line
+ `--json` `impact` field; `record_ask_usage` returns the impact (reuses the counterfactual it
already computed for telemetry — no extra query). Honest: compares vs the **cited** files, not
the whole repo.

**Remote sources (v0.32, "reach"):** `indexa pack add-url <pack> <url>` fetches a GitHub issue/PR (public
API → Markdown) or web page (HTML→Markdown via `html2md`, `<script>`/`<style>` stripped) → caches a local
file under `<data_dir>/sources/<slug>-<sha8>.md` → `add_pack_paths` (cache-as-file: NO schema change, NO
virtual entries — flows through the normal pipeline). Code in `apps/indexa/src/commands/sources.rs` (uses
`indexa_http_util`). **Gated** by `[sources] enabled` (`SourcesConfig`) OR `INDEXA_REMOTE_FETCH_ALLOW=1` —
fetching is opt-in (network). arXiv/YouTube/site scrapers stay OUT of core (rot → plugins). Pack "recipes"
deferred.

**Major features by version:** Context Packs (v0.14) · Importance Weighting (v0.16, `importance_weights`
table + `boost_with_weights` in QA) · Insights (v0.16, `find_*_duplicates`/`find_stale_entries`/
`weekly_diff`) · video captioning (v0.16, `parsers.video`) · Plugin SDK (v0.15, `indexa_parsers::Registry`
+ `register()`) · LAN serve (v0.16) · **signature graph visualization** (v0.18, `store.code_graph` →
`/api/graph` → Map tab "Graph" sub-view, force-directed SVG) · **PageRank centrality** (v0.20,
`store::pagerank` weighted PageRank → `CodeGraphNode.pagerank`; Map graph sizes nodes by centrality;
`indexa graph` / `code_graph` MCP list hub files) · **agentic `ask`** (v0.20, `indexa ask --agentic` /
MCP `agentic` / web "Agentic" checkbox — bounded plan→search→refine loop, fails open) · **universal
macOS desktop build** (v0.20, `--target universal-apple-darwin`, `darwin-universal` updater key).

**MCP server:** **46 tools** (`crates/mcp/src/lib.rs`; incl. `list_supported_formats`). Code-graph tools: `dependencies` /
`who_imports` / `who_calls` / `blast_radius` / `code_graph`. The call graph is bare-name matched
(case-sensitive, 1-hop, 8 languages: Rust/Python/JS/TS/Go/Java/C/C++) — caveats in `docs/methodology.md`; label honestly in any UI.
v0.28 added `query_config` (effective config, no secrets), `list_files_by_category` (classification
category → files), `get_chunk_context` (a file's indexed chunks / neighbors of a search hit), plus
`offset` pagination on `list_open_decisions`. **v0.64** added a separate Resources + Prompts surface
(does NOT change the 46-tool count): **4 resources** (`indexa://overview · packs · pack/{name} ·
summary/{path}`) + **3 prompts** (`onboarding-overview · explain-file · pack-context`) in
`crates/mcp/src/{resources,prompts}.rs`, golden-listed in `golden_prompts.txt`; the `ask` tool gained
an optional `session_id` (Conversational Ask).

**Web UI:** pure vanilla JS + SVG (`createElementNS`), zero frontend libraries. JS/CSS are
`include_str!`-concatenated in `crates/web/src/lib.rs` — a new `NN-name.js`/`.css` must be added to
that concat list or it is dead. Bundle contains emoji → use `grep -a` when searching it.

## Verification before declaring done

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release
```

For UI changes: `indexa serve` then visually confirm in browser at http://localhost:7620. When the
Claude Chrome extension is unavailable, verify with a zero-dep headless-Chrome CDP harness (Node 24
`WebSocket`+`fetch`, launches `--headless=new`, drives the page over CDP) — see
`memory/feedback_browser_verification.md`.

## Git workflow

This is in the `harf-promo` org (public repo; branch protection on `main` requires a PR + green CI). **Never push directly to `main`.** Always:
1. `git checkout -b <short-feature-name>`
2. Commit with sign-off (`git commit -s`) — the DCO workflow requires `Signed-off-by` on every commit
3. Push the branch; open a PR; squash-merge on green CI

**If commits on a branch are missing sign-off:** `git rebase --signoff origin/main` then `git push --force-with-lease`.

**Branch protection is active on main:** requires `fmt + clippy + test` (ubuntu/macos/windows), `License and advisory check`, and `DCO sign-off check`. Force-push and deletion are blocked.

## Multi-pass refinement defaults (v0.2.3+)

`--passes` default: **2 for first-time summarization, 1 for refresh** (existing summary row present). Hard cap: 3. Based on Self-Refine (Madaan et al., NeurIPS 2023) — gain saturates pass 2→3, degrades at pass 4+.

## Security invariants

- `POST /api/keys` gated by `INDEXA_WEB_ALLOW_KEY_EDIT=1`; config file written at 0600; keys never logged.
- Cross-compile: all `reqwest` users use `default-features = false, features = ["rustls-tls"]`.

## File-type classification priority (v0.2.3+)

1. Exact filename hit (Linguist `FILENAMES` phf_map)
2. Extension hit (Linguist `EXTENSIONS` phf_map)
3. Ambiguous extensions → `hyperpolyglot::detect(path)` (shebang + content heuristics)
4. MIME fallback (`mime_guess`)

## One-shot indexing

`indexa index <path>` runs scan → deep → summarize in one command. Use this instead of the three-step pipeline for first-time builds or complete refreshes.

## Desktop app

The Tauri desktop app is **excluded from `cargo --workspace`** (webkit2gtk missing on CI runners). Build it separately:
```bash
cargo build --manifest-path apps/indexa-desktop/Cargo.toml
```
CI for the desktop uses the release workflow, not the standard CI workflow.

## Index database path (macOS)

```
~/Library/Application Support/dev.indexa.Indexa/index.db
```

(Per-platform paths are tabled in `USAGE.md` §2 — Linux uses `~/.local/share/indexa/`.)

Quick queue health check:
```bash
sqlite3 "$HOME/Library/Application Support/dev.indexa.Indexa/index.db" \
  "SELECT state, COUNT(*) FROM summary_queue GROUP BY state"
```

## Release procedure

1. Branch: `git checkout -b bump-X.Y.Z`
2. Bump `version = "X.Y.Z"` in **both** `Cargo.toml` (workspace root) and `apps/indexa-desktop/Cargo.toml`
3. `git commit -s -m "chore: bump version to X.Y.Z"`
4. PR → squash-merge on green CI
5. `git checkout main && git pull && git tag vX.Y.Z && git push origin vX.Y.Z`
6. Release CI auto-triggers: builds 5 binary targets + Apple Silicon Tauri `.dmg`

The `.dmg`/`.app` are **Developer ID signed + notarized** when the Apple secrets are present
(ad-hoc fallback otherwise) — see [`docs/signing.md`](docs/signing.md) for the required GitHub
secrets and how to obtain them.
