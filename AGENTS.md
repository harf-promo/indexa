<!-- fleet-template: v1 | reconciled-against: fleet-kit/templates/AGENT-CONTEXT-TEMPLATE.md @ 35354d0 2026-09-07 -->
# Indexa — agent contract

Feature history lives in `CHANGELOG.md` — do not narrate versions here. This file holds only the pitch, the invariants, and the procedures.

## What this repo is

Indexa is **the local context engine for AI**. The index is the substrate; context is the product. Never revert to "file indexer" framing in user-facing copy. Two audiences, one engine: it saves **cloud** AI tools their paid token budget, *and* gives **local** models the context they can't hold in a small window — by serving a retrieved slice instead of the whole repo. **Context Packs** = subject-scoped, named, exportable bundles (XML/Markdown, never HTML). The name stays **Indexa**; the tagline carries "context" — a settled naming/positioning decision, not open for revisiting.

## Stack & layout

**Rust/cargo** workspace with vanilla **JavaScript**, **Tauri** desktop shell and **MCP** tool/resource/prompt server. No `package.json`/`pyproject.toml` exists in the tree (`stack.package_manager: cargo`). Web JS/CSS is `include_str!`-concatenated in `crates/web/src/lib.rs`; there is no separate JS build/bundle command.

- `crates/core/` — shared domain types
- `crates/query/` — retrieval, ranking, QA (`qa/retrieve.rs`)
- `crates/embed/` — embedding pipeline
- `crates/parsers/` — the ~84-format file parsers
- `crates/llm/` — local-model (Ollama) integration
- `crates/mcp/` — MCP server; `tool_router()` composes router modules, not one `lib.rs` (hot file)
- `crates/http-util/` — shared HTTP client, rustls-only
- `crates/web/` — the `:7620` web UI, `include_str!`-concatenated JS/CSS (hot file)
- `crates/update/` — in-app updater; bridges Rust→web over SSE, no Tauri IPC
- `apps/indexa/` — the CLI binary (`main.rs` is a hot file)
- `apps/indexa-desktop/` — Tauri app; **workspace-excluded**, own committed `Cargo.lock`
- `tools/gen-fingerprints/` — generator for the fingerprint matcher

## Local models required

Before configuring Ollama or indexing/Q&A, read the [model pull commands and sizes](docs/agents/operations.md#local-models-required).

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

These preserve the repo's dev loop and [.orchestration/lanes.yml](.orchestration/lanes.yml)'s verify lane. Use the combined command to reproduce CI; plain `cargo test` is narrower.

## Feature surface (timeless — details in CHANGELOG.md)

- **MCP server:** 56 tools across router modules in `crates/mcp` composed in `tool_router()` (NOT one lib.rs), + 4 resources (`indexa://…`) + 3 prompts. A pinned test (`doc_tool_count_matches_code`) keeps this number honest — update it when tools change.

Before changing a feature, read the [full feature surface](docs/agents/operations.md#feature-surface-timeless--details-in-changelogmd).

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

Before validation or dependency edits, read [verification](docs/agents/verification.md#verification-before-done) for CI triggers and lockfile rules. Changes anywhere in the desktop dependency graph require regenerating and reviewing its separate lockfile without floating pinned versions.

UI changes require `indexa serve` and a real browser-preview/screenshot at http://localhost:7620 on this VPS using headless Chrome + Xvfb and the chrome-devtools MCP server. Markdown changes require the [offline link/fragment check](docs/agents/verification.md#markdown-links).

## Guardrails

**Tier: `pr-preferred`**, per `~/.claude/hooks/billed_repos.json`, key `harf-promo/indexa`; branch protection applies without exception.

- **Shared MCP/UI files and counts:** before editing them, read [Shared files and counters](docs/agents/verification.md#shared-files-and-counters) and [.orchestration/lanes.yml](.orchestration/lanes.yml)'s `hot_files`/`counters`. Never hand-edit a tool-count number in prose without updating the code; `doc_tool_count_matches_code` enforces the root claim above.
- **Key-edit endpoint:** `POST /api/keys` is gated behind `INDEXA_WEB_ALLOW_KEY_EDIT=1`; the config file is `0600`; keys are never logged. Treat any change here as a security-sensitive path needing explicit confirmation.
- **No Supabase / RLS / payment surface exists** (`supabase.present: false`).
- **No merge-triggered deploy:** `.orchestration/lanes.yml` has `deploy_on_merge: []`; `release.yml` triggers only on `vX.Y.Z` tag pushes. Follow Release procedure below.

## Git & PR flow

Public repo in `harf-promo`; branch protection on `main` (PR + green CI: fmt/clippy/test on 3 OSes, license check, DCO). **Never push directly to main.**

1. `git checkout -b <short-feature-name>`
2. `git commit -s` (DCO Signed-off-by required on every commit)
3. Push → PR → squash-merge on green. **No force push** (`force_push: blocked`). For missing sign-offs on a published branch, create a new branch from updated `origin/main`, replay the intended commits with `git cherry-pick -s <commit>...`, and push the new branch for a replacement PR. Do not rewrite the published branch.

The global `git-safety-guard.py` hook covers all `billed_repos.json` repos, including Indexa; no additional repo-local `block-main-commit` hook exists or is needed in `.claude/settings.json`. `.orchestration/lanes.yml` declares `merge: squash`, `force_push: blocked`, `required_checks: [ci]`. No repo-specific shipper exists; use `/ship`.

## Operational facts

Before indexing, changing classification/multi-pass defaults, working on desktop packaging, or inspecting the index/summary queue, read [Operational facts](docs/agents/operations.md#operational-facts).

## Release procedure

Before bumping versions, tagging or publishing, read the complete [Release procedure](docs/agents/release-and-hosts.md#release-procedure). Bump BOTH root `Cargo.toml` and `apps/indexa-desktop/Cargo.toml` through a signed-off PR; release is triggered only by a version tag after green squash-merge.

## Host boundaries — VPS vs Mac

Nothing in this repo is Mac-only (`mac_only_paths: []`): no `ios/`, Xcode project or Expo/EAS step. Tauri is cross-platform; a local desktop build needs Linux system deps including webkit2gtk. Release binaries and the signed/notarized Apple Silicon `.dmg` build on GitHub-hosted multi-OS runners.

Web preview belongs on this VPS (headless Chrome + Xvfb + chrome-devtools MCP). Xcode, iOS/Android simulators and native mobile builds are unavailable here and do not apply to Indexa. Before desktop builds, signing, or deciding where verification runs, read [Host boundaries](docs/agents/release-and-hosts.md#host-boundaries--vps-vs-mac).

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
| Configuring models, changing features/defaults, or inspecting the index | [Operations](docs/agents/operations.md); MCP/retrieval internals: `crates/mcp/src/lib.rs`, `crates/query/src/qa/retrieve.rs` |
| Validating changes, editing dependencies/shared files/counts | [Verification](docs/agents/verification.md), [.orchestration/lanes.yml](.orchestration/lanes.yml) (`hot_files`, counters, required checks) |
| Releasing, signing, building desktop, or choosing a verification host | [Release and hosts](docs/agents/release-and-hosts.md), [signing](docs/signing.md) (.dmg notarization) |
| Using CLI commands or finding per-platform index DB paths | [USAGE.md](USAGE.md) |
| Using MCP over stdio / live retrieval | [Walkthrough](docs/how-to/live-retrieval-over-mcp.md) |
| Recording or checking version history | [CHANGELOG.md](CHANGELOG.md) |
| Reconciling fleet context | `fleet-kit/templates/AGENT-CONTEXT-TEMPLATE.md` |

Before editing, discover instructions with `rg --files --hidden -g '!.git' -g AGENTS.md -g CLAUDE.md`. This tree has no deeper `AGENTS.md`/`CLAUDE.md` (fleet-command's `nested.json` records the inventory). Root `CLAUDE.md` stays exactly `@AGENTS.md` plus a trailing newline (11 bytes); never duplicate guidance there. Read any newly discovered nested instructions for the paths they cover.

## Fleet context

This file follows the fleet-wide template
(`fleet-kit/templates/AGENT-CONTEXT-TEMPLATE.md`, stamped above). Config drift
between this file and the template is caught automatically by `fleet-doctor.sh`, which
runs as part of fleet-command's daily sweep — see that repo's `PORTFOLIO.md` and
`SWEEP.md` for what gets reported and what (if anything) gets auto-dispatched.
