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

## Current feature surface (v0.39.0)

**CLI commands** (`indexa <cmd>`): `index` (one-shot scan→deep→summarize) · `scan` · `deep` ·
`summarize` · `describe` · `inspect` (per-path "what's indexed here") · `map` · `worker` · `pack`
(Context Packs; `pack add-url` = remote sources) · `weight` (Importance
weighting) · `insights` (duplicates/stale/diff) · `graph` (file-to-file call graph) · `export` ·
`ask` · `watch` · `serve` (`--host 0.0.0.0` for LAN) · `mcp` (+ `mcp install [--client]`, auto-detects)
· `completion <shell>` · `status` (`--json` incl. per-tool savings) · `rm` · `prune` (orphan-row GC) ·
`doctor` (`--apply-ollama-env`) · `fingerprint` · `classify` · `update`.

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
retrieved-slice model is the moat (vs repomix/gitingest token bricks).

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

**MCP server:** **42 tools** (`crates/mcp/src/lib.rs`). Code-graph tools: `dependencies` /
`who_imports` / `who_calls` / `blast_radius` / `code_graph`. The call graph is bare-name matched
(case-sensitive, 1-hop, 7 languages) — caveats in `docs/methodology.md`; label honestly in any UI.
v0.28 added `query_config` (effective config, no secrets), `list_files_by_category` (classification
category → files), `get_chunk_context` (a file's indexed chunks / neighbors of a search hit), plus
`offset` pagination on `list_open_decisions`.

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

This is in the `harf-promo` org (private repo, free-tier Actions minutes). **Never push directly to `main`.** Always:
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
