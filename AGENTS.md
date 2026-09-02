<!-- fleet-template: v1 | reconciled-against: fleet-command/docs/fleet/AGENT-CONTEXT-TEMPLATE.md @ c0c8dd5 2026-09-02 -->
# Indexa — agent contract

Feature history lives in `CHANGELOG.md` — do not narrate versions here. This file holds only the pitch, the invariants, and the procedures.

## What this repo is

Indexa is **the local context engine for AI**. The index is the substrate; context is the product. Never revert to "file indexer" framing in user-facing copy. Two audiences, one engine: it saves **cloud** AI tools their paid token budget, *and* gives **local** models the context they can't hold in a small window — by serving a retrieved slice instead of the whole repo. **Context Packs** = subject-scoped, named, exportable bundles (XML/Markdown, never HTML). The name stays **Indexa**; the tagline carries "context" — a settled naming/positioning decision, not open for revisiting.

## Stack & layout

**Rust** workspace (package manager: **cargo** — no `package.json`/`pyproject.toml` anywhere in the tree, per fleet facts `stack.package_manager`), plus a small amount of vanilla **JavaScript** for the web UI. Frameworks: **Tauri** (desktop shell) and **MCP** (the tool/resource/prompt server). The web UI's JS/CSS is `include_str!`-concatenated straight into `crates/web/src/lib.rs` — there is no separate JS build/bundle command (see Load-bearing invariants below).

- `crates/core/` — shared domain types
- `crates/query/` — retrieval, ranking, QA (`qa/retrieve.rs` — see Load-bearing invariants)
- `crates/embed/` — embedding pipeline
- `crates/parsers/` — the ~84-format file parsers
- `crates/llm/` — local-model (Ollama) integration
- `crates/mcp/` — the MCP server; tools are composed in `tool_router()` across router modules, not one `lib.rs` (hot file — see Guardrails)
- `crates/http-util/` — shared HTTP client, rustls-only (see Load-bearing invariants)
- `crates/web/` — the `:7620` web UI, `include_str!`-concatenated JS/CSS (hot file)
- `crates/update/` — in-app updater; bridges Rust→web over SSE, no Tauri IPC
- `apps/indexa/` — the CLI binary (`main.rs` is a hot file)
- `apps/indexa-desktop/` — the Tauri desktop app; **workspace-excluded**, own committed `Cargo.lock` (see Operational facts and Host boundaries below)
- `tools/gen-fingerprints/` — generator for the fingerprint matcher

Full feature detail is in `## Feature surface` below — this section is deliberately just the map.

## Local models required

```bash
ollama pull nomic-embed-text   # embedding (~270 MB)
ollama pull gemma3:4b          # file summaries (~2.5 GB)
ollama pull gemma3:12b         # dir roll-ups + Q&A (~8 GB)
```

## Commands

```bash
# format
cargo fmt --check

# lint
cargo clippy --workspace -- -D warnings

# test
cargo test --workspace

# build
cargo build --release
cargo build --manifest-path apps/indexa-desktop/Cargo.toml   # desktop app is workspace-excluded — build it explicitly

# verify — exactly what CI runs (.orchestration/lanes.yml's verify lane)
cargo fmt --all -- --check && cargo clippy --all-targets --all-features --locked && cargo test --all --locked

# dev
indexa serve   # starts the web UI (:7620) + local MCP server
```

All of these are grounded in this repo's own `AGENTS.md` (the four dev-loop commands and `indexa serve`) or `.orchestration/lanes.yml`'s `verify` lane (the CI-matching combined command, which additionally locks and covers all targets/features — use it, not the plain `cargo test`, when you need to reproduce what CI actually checks). Nothing here is invented; nothing from fleet facts' `commands` list was dropped.

## Feature surface (timeless — details in CHANGELOG.md)

- **MCP server:** 53 tools across router modules in `crates/mcp` composed in `tool_router()` (NOT one lib.rs), + 4 resources (`indexa://…`) + 3 prompts. A pinned test (`doc_tool_count_matches_code`) keeps this number honest — update it when tools change.
- **Retrieval:** hybrid BM25/FTS5 + dense embeddings, RRF fusion, archive/code-intent/recency boosts, rerank, MMR; eval-gated via `indexa eval` over `fixtures/self-golden.json`.
- **Ask:** grounded RAG; `synthesize:false` returns the raw slice; conversational via `session_id`; `explain_retrieval` traces scoring.
- **Context Packs** (create/add/remove/export/search, remote `add-url` opt-in; exports secret-redacted) · **code graph** (deps/who_imports/who_calls/blast_radius; 8 languages, 1-hop, case-sensitive) · **decision-review ledger** (durable, patch-id-anchored notes via `record_decision`) · **summarize-pass drift** (stale/orphaned/uncovered) · **classification + importance weights** · **savings/impact accounting** (≈4 bytes/token estimate).
- **Web UI** at :7620 · **Tauri desktop app** (in-app updater) · **CLI** (`index scan deep summarize … doctor eval`).
- **Parsers:** ~84 formats incl. Office, PDF (+opt-in OCR), EPUB, email, iWork, archives, opt-in multimodal.

## Load-bearing invariants — do not "fix" or remove

- **Web UI:** pure vanilla JS + SVG, zero frontend libraries. JS/CSS are `include_str!`-concatenated in `crates/web/src/lib.rs` — a new `NN-name.js`/`.css` MUST be added to that concat list or it is dead. Bundle contains emoji → `grep -a`. Syntax highlighter stays a client-side dependency-free tokenizer (tree-sitter-highlight conflicts with the parsers' tree-sitter 0.26).
- **Memory budget:** `resource::compute_budget` keys on `available_bytes`, NOT `total − used_memory()` (sysinfo counts the macOS compressor). Don't reintroduce the `micro_benchmark` dead field.
- **Retrieval boosts:** `retrieve()` in `crates/query/src/qa/retrieve.rs` applies `apply_archive_penalty` (×0.15 on archive/archived/historical/deprecated/old segments) and `apply_code_intent_boost` (×1.6). Removing them makes answers cite `docs/archive/` and claim unshipped versions.
- **openssl-free tree:** all `reqwest` users pin `default-features = false, features = ["rustls"]` (reqwest 0.13 renamed `rustls-tls` → `rustls`, and its rustls backend now defaults to the aws-lc-rs crypto provider + `rustls-platform-verifier` OS trust store, not the 0.12-era webpki-roots/ring combo); hf-hub pins `["ureq"]`. Verify: `cargo tree -i openssl-sys --target aarch64-unknown-linux-gnu` must be empty.
- **Verified non-bugs — don't "fix":** `trim_continuation` slice, `delete_subtree` prefix, redact count.
- **Web boot:** call the bare hoisted `restoreFromHash` from `08`'s boot, NOT `window.__indexaRestoreHash` (assigned later, in `26`).
- **`crates/web/src/update_control.rs`:** copy the value out before `send(None)` or it self-deadlocks. Update progress bridges Rust→web over SSE without Tauri IPC; `crates/update` stays web-agnostic (no circular dep).
- **Fingerprint matcher:** hand-written `*`/`?` glob — do NOT promote to `globset`; `**` rejected.
- **`directory_apps`:** persistence follows the classifications lifecycle; orphan-guard tests must include it in `orphan_rows_for`/`seed_full_entry`; app-detection runs as a SIBLING of `run_detectors`, not folded in.
- **Concurrency:** the qa crate takes conversation history as `&[PriorTurn]` by value so `&Store` never crosses `.await`.
- **CLI-skew detection:** `parse_plist_short_version` anchors the exact `<key>CFBundleShortVersionString</key>` key (loose "Version" grabs the wrong dict entry). doctor/status/MCP are authoritative; desktop marker + web banner secondary. Restart the MCP server after a CLI update.

## Verification before done

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release
```

**Touched a dependency anywhere in the graph?** `apps/indexa-desktop` is workspace-excluded with its own committed `Cargo.lock` — CI builds it `--locked`. Adding/removing a dep in a crate the desktop app pulls in (even transitively) leaves that lock stale and red-Xs `desktop build (macOS)` with no local signal, since `cargo build --workspace` never touches it. Regenerate it: `cargo generate-lockfile --manifest-path apps/indexa-desktop/Cargo.toml`, then diff it — only your actual dependency change should move; the pinned `brotli`/`pcre2` versions (see that Cargo.toml's own comment) must not float.

UI changes: `indexa serve` → visually confirm at http://localhost:7620 (headless Chrome + Xvfb + the chrome-devtools MCP server are available on this VPS for that check — see the fleet-wide host CLAUDE.md).

CI (github-hosted runners, per fleet facts `ci.workflows`) runs six workflows on every push/PR: `ci.yml` (fmt/clippy/test on 3 OSes — see Git & PR flow below), `cargo-deny.yml` (license/dependency policy), `dco.yml` (sign-off check), `dense-eval.yml` (the retrieval eval gate over `fixtures/self-golden.json`), `docs.yml`, and `release.yml` (tag-triggered only — see Release procedure below, not run on merges to `main`).

**Host note (this VPS, wikiclaw-1):** headless Chrome + Xvfb and the `chrome-devtools` MCP server are available on this host (confirmed 2026-08-31) — do the `indexa serve` → `http://localhost:7620` visual confirmation above directly from here via a real browser-preview/screenshot, rather than deferring it to the Mac. The only things genuinely unavailable on this VPS are Xcode, iOS/Android simulators, and native mobile builds — none of which apply to indexa (the desktop app is Tauri, with no iOS/Android target).

## Guardrails

**Tier: `pr-preferred`** (per `~/.claude/hooks/billed_repos.json`, key `harf-promo/indexa` — not `billed`, but the branch-protection rule below still applies without exception).

- **MCP/UI tool-count tokens are hot files.** `.orchestration/lanes.yml`'s `hot_files` list flags `crates/mcp/src/lib.rs`, `crates/web/src/lib.rs`, `AGENTS.md`, `README.md`, `USAGE.md`, `docs/how-to/live-retrieval-over-mcp.md`, `CHANGELOG.md`, `crates/cli/src/lib.rs`, and `apps/indexa/src/main.rs` as pure-addition edit targets shared by multiple lanes — expect a trivial rebase there, not a logic conflict. Never hand-edit a tool-count number in prose without also updating the code: `lanes.yml`'s `counters` are `mcp_tool_count: 53`, `ui_js_fragment_count: 32`, `ui_css_fragment_count: 23`; the pinned `doc_tool_count_matches_code` test enforces the MCP one (see Feature surface above) and is exactly the kind of check that silently breaks if the doc and the code drift.
- **Key-edit endpoint:** `POST /api/keys` is gated behind `INDEXA_WEB_ALLOW_KEY_EDIT=1`; the config file is `0600`; keys are never logged (full detail in Operational facts below). Treat any change here as a security-sensitive path needing explicit confirmation.
- **No Supabase / RLS / payment surface exists in this repo** (`supabase.present: false` in fleet facts) — there is nothing of that shape to guard here.
- **No merge-triggered deploy.** `.orchestration/lanes.yml`'s `deploy_on_merge` is empty — the only release trigger is pushing a `vX.Y.Z` tag (`release.yml`; see Release procedure below), never a merge to `main`.
- See **Load-bearing invariants** above for the specific code paths (memory budget calc, retrieval boosts, the openssl-free tree, etc.) that must never be silently "fixed" or removed.

## Git & PR flow

Public repo in `harf-promo`; branch protection on `main` (PR + green CI: fmt/clippy/test on 3 OSes, license check, DCO). **Never push directly to main.**
1. `git checkout -b <short-feature-name>`
2. `git commit -s` (DCO Signed-off-by required on every commit)
3. Push → PR → squash-merge on green. Missing sign-offs: `git rebase --signoff origin/main` + `git push --force-with-lease`.

Enforced by the host-level `git-safety-guard.py` hook (global, covers every repo listed in `billed_repos.json`, including this one) — this repo carries no repo-local `block-main-commit`-style hook in `.claude/settings.json`, and none is needed on top of that global backstop. `.orchestration/lanes.yml` additionally declares `merge: squash`, `force_push: blocked`, and `required_checks: [ci]`. No repo-specific shipper skill exists for indexa; use `/ship`.

## Operational facts

- **Multi-pass defaults:** `--passes` = 2 first-time, 1 refresh, hard cap 3 (Self-Refine: gains saturate at pass 3).
- **Security:** `POST /api/keys` gated by `INDEXA_WEB_ALLOW_KEY_EDIT=1`; config file 0600; keys never logged.
- **Classification priority:** filename phf_map → extension phf_map → `hyperpolyglot::detect` → MIME fallback.
- **One-shot indexing:** `indexa index <path>` = scan → deep → summarize; use for first builds/full refreshes.
- **Desktop app:** excluded from `cargo --workspace` (webkit2gtk absent on CI); build via `cargo build --manifest-path apps/indexa-desktop/Cargo.toml`; released by the release workflow, not standard CI.
- **Index DB (macOS):** `~/Library/Application Support/dev.indexa.Indexa/index.db` (other platforms: `USAGE.md` §2). Queue health: `sqlite3 "$HOME/Library/Application Support/dev.indexa.Indexa/index.db" "SELECT state, COUNT(*) FROM summary_queue GROUP BY state"`.

## Release procedure

1. `git checkout -b bump-X.Y.Z`; bump `version` in BOTH root `Cargo.toml` and `apps/indexa-desktop/Cargo.toml`
2. `git commit -s -m "chore: bump version to X.Y.Z"` → PR → squash-merge on green
3. `git checkout main && git pull && git tag vX.Y.Z && git push origin vX.Y.Z`
4. Release CI builds 5 binary targets + Apple Silicon `.dmg` (Developer ID signed + notarized when Apple secrets present — `docs/signing.md`).

## Host boundaries — VPS vs Mac

Nothing in this repo is Mac-only — `mac_only_paths: []` in fleet facts, and indexa has no `ios/` directory, no Xcode project, and no Expo/EAS step anywhere. The Tauri desktop app (`apps/indexa-desktop`) is genuinely cross-platform: its 5 release binary targets — including the Apple Silicon `.dmg` (Developer ID signed + notarized, `docs/signing.md`) — are built by `release.yml` on GitHub-hosted multi-OS runners, not on this VPS, so nothing about that pipeline needs to run here. A *local* desktop build attempted on this box (`cargo build --manifest-path apps/indexa-desktop/Cargo.toml`) needs Tauri's Linux system deps (webkit2gtk etc. — the same reason it's excluded from `cargo build --workspace` on CI, per Operational facts above); that's a Linux-packaging concern, not a Mac-only one.

Web UI verification is a VPS strength here, not a gap: headless Chrome + Xvfb and the `chrome-devtools` MCP server are available on this host (confirmed 2026-08-31) — browser-preview and screenshot checks for the `:7620` web UI can and should happen from this VPS directly (see the host note under Verification before done above). The only things genuinely unavailable on this host are Xcode, iOS/Android simulators, and native mobile builds — irrelevant to indexa, which ships a desktop app, not a mobile one.

## Orca conventions

- Update the worktree comment at meaningful checkpoints:
  `orca-ide worktree set --worktree active --comment "<status>" --json`
- Set `--workspace-status in-review` when a PR opens on this repo's work.
- A dispatched worker sends `worker_done` exactly once, with an explicit
  `--outcome`, when finishing supervised orchestration work here — see
  fleet-command's `ORCHESTRATION.md` for the full coordinator recipe.

## Where to find more

| Topic | Where |
| --- | --- |
| MCP tool/resource/prompt surface, retrieval internals | `## Feature surface` above, `crates/mcp/src/lib.rs`, `crates/query/src/qa/retrieve.rs` |
| Full CLI/usage reference, per-platform index DB paths | `USAGE.md` |
| MCP-over-stdio / live-retrieval walkthrough | `docs/how-to/live-retrieval-over-mcp.md` |
| Release signing (.dmg notarization) | `docs/signing.md` |
| Branch-merge machine config (hot_files, counters, required checks) | `.orchestration/lanes.yml` |
| Version history | `CHANGELOG.md` |
| Fleet-wide template this file follows | `fleet-command/docs/fleet/AGENT-CONTEXT-TEMPLATE.md` |

The only nested context file below the repo root is the root `CLAUDE.md` stub itself (`@AGENTS.md`, 11 bytes) — there is no deeper nested `AGENTS.md`/`CLAUDE.md` anywhere in this tree, so it's left as-is (see fleet-command's `nested.json` for this repo).

## Fleet context

This file follows the fleet-wide template
(`fleet-command/docs/fleet/AGENT-CONTEXT-TEMPLATE.md`, stamped above). Config drift
between this file and the template is caught automatically by `fleet-doctor.sh`, which
runs as part of fleet-command's daily sweep — see that repo's `PORTFOLIO.md` and
`SWEEP.md` for what gets reported and what (if anything) gets auto-dispatched.
