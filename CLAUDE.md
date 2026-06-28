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

## Current version

v0.75.0 — per-release detail lives in `CHANGELOG.md`.

**CLI commands** (`indexa <cmd>`): `index` (one-shot scan→deep→summarize; `--contextual` flag) · `scan` · `deep` (`--contextual` flag) ·
`summarize` · `describe` (no-path = whole-project overview; with path = per-file summary) · `inspect` (per-path "what's indexed here") · `map` · `worker` · `pack`
(Context Packs; `pack add-url` = remote sources) · `weight` (Importance
weighting) · `insights` (duplicates/stale/diff) · `graph` (file-to-file call graph) · `export` ·
`ask` · `watch` · `serve` (`--host 0.0.0.0` for LAN) · `mcp` (+ `mcp install [--client]`, auto-detects)
· `completion <shell>` · `status` (`--json` incl. per-tool savings) · `rm` · `prune` (orphan-row GC) ·
`doctor` (`--apply-ollama-env`) · `fingerprint` · `classify` · `update`.

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

## Invariants (do not regress)

These are hard constraints discovered through bugs or adversarial review. None may be silently removed.

- **Memory budget:** `resource::compute_budget` uses `available_memory()` (sysinfo — macOS XNU active+inactive+free), NOT `total − used_memory()`. sysinfo 0.39's `used_memory()` includes the macOS **compressor**, so `total−used` falsely refuses models. Do NOT reintroduce the `total−used` basis or the removed `micro_benchmark` config field. → `[[project-resource-engine]]`

- **Answer quality:** Keep `apply_archive_penalty` (HISTORICAL_SEGMENTS ×0.15), `apply_code_intent_boost` (×1.6 on code-intent questions), and `trim_continuation` in `crates/query/src/qa.rs`. All three are always-on. The 3 "bugs" a past adversarial agent flagged (`trim_continuation` slice, `delete_subtree` prefix, redact count) were **verified false positives** — do not "fix" them.

- **Update-progress bridge:** Rust→web progress uses the `watch` channel in `crates/web/src/update_progress.rs` (SSE via `GET /api/update/progress/stream`), NOT Tauri IPC — the webview loads a remote URL and has no `withGlobalTauri`. In `update_control.rs` `wait_for_command`: copy the value out **before** `send(None)` or it self-deadlocks.

- **Export safety:** `redact_secrets` runs on ALL four export surfaces (CLI `export`/`pack export`, MCP `export_pack`, web `api_export`) before content leaves the machine. Empty slice → loud failure (CLI bail / web 404|422), never a silent empty artifact. Do NOT add a whole-repo "dump" mode — the retrieved-slice model is the moat.

- **openssl-free / TLS:** all `reqwest` users `default-features = false, features = ["rustls-tls"]`. `hf-hub` pinned to `0.5` with `default-features = false, features = ["ureq"]` (ureq 3 rustls+ring TLS, NOT native-tls/openssl). Verify any hf-hub change: `cargo tree -i openssl-sys --target aarch64-unknown-linux-gnu` must be empty.

- **MCP tool count = 46** (`crates/mcp/src/lib.rs`). Extend only via optional params; never casually add/remove a tool. After each CLI update the MCP server must be restarted — it spawns the new binary; version skew is device-only/user-triggered.

- **Web UI build:** zero frontend libs; a new `NN-name.js`/`.css` is dead until added to the `include_str!` concat in `crates/web/src/lib.rs`. File-preview syntax highlighting is a self-written client tokenizer (keyword/string/comment/number → `.hl-*` tokens) — NOT tree-sitter-highlight (no 0.26-compatible release); keep it client-side + dependency-free.

- **Fingerprint glob matcher:** tiny hand-written `*`/`?` matcher in `crates/core/src/fingerprint.rs`. `**` markers are rejected (not silently coerced to `*`). Do NOT promote `globset`.

- **URL restore in `26-url-state.js`:** call the BARE hoisted name `restoreFromHash` from `08`'s boot, NOT `window.__indexaRestoreHash` — the window assignment runs later in `26` and is not yet set at boot time.

- **Commit sign-off:** `git commit -s` (DCO required; branch protection enforces it). Run commits in FOREGROUND Bash — 1Password blocks background SSH-agent signing. `gh run watch --exit-status` is unreliable; confirm CI with `gh pr checks`.

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
