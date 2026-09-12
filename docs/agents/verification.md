Read this before validation, any dependency change, or edits to shared MCP/UI files and count claims.

# Indexa verification and shared files

Run commands from the repository root. The always-loaded rules remain in [AGENTS.md](../../AGENTS.md).

## Verification before done

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release
```

The root [Commands](../../AGENTS.md#commands) also retain the explicit desktop build and the combined locked, all-targets/features verify command from [.orchestration/lanes.yml](../../.orchestration/lanes.yml). Use that combined command when reproducing CI; plain `cargo test` is narrower.

**Touched a dependency anywhere in the graph?** `apps/indexa-desktop` is workspace-excluded with its own committed `Cargo.lock` — CI builds it `--locked`. Adding/removing a dep in a crate the desktop app pulls in (even transitively) leaves that lock stale and red-Xs `desktop build (macOS)` with no local signal, since `cargo build --workspace` never touches it. Regenerate it: `cargo generate-lockfile --manifest-path apps/indexa-desktop/Cargo.toml`, then diff it — only your actual dependency change should move; the pinned `brotli`/`pcre2` versions (see that Cargo.toml's own comment) must not float.

UI changes: `indexa serve` → visually confirm at http://localhost:7620 (headless Chrome + Xvfb + the chrome-devtools MCP server are available on this VPS for that check — see the fleet-wide host CLAUDE.md).

CI uses GitHub-hosted runners (`ci.workflows` in fleet facts). The six workflows have these distinct triggers; they do not all run on every push/PR:

| Workflow | Trigger and checks |
| --- | --- |
| [ci.yml](../../.github/workflows/ci.yml) | Push to `main` and PR targeting `main`: fmt/clippy/test on 3 OSes, headless web smoke, hermetic sparse retrieval eval over `fixtures/self-golden.json`, and a separate locked desktop build on macOS. |
| [cargo-deny.yml](../../.github/workflows/cargo-deny.yml) | Push to `main` and PR targeting `main`: license/dependency policy. |
| [dco.yml](../../.github/workflows/dco.yml) | PR targeting `main`: sign-off check; follow the root [Git & PR flow](../../AGENTS.md#git--pr-flow). |
| [dense-eval.yml](../../.github/workflows/dense-eval.yml) | Manual `workflow_dispatch` only: dense/RRF retrieval eval over `fixtures/self-golden.json` using Ollama and real embeddings. |
| [docs.yml](../../.github/workflows/docs.yml) | PR changing `**/*.md`: offline Markdown file/fragment links. |
| [release.yml](../../.github/workflows/release.yml) | Push of `v*.*.*` tags only; never a merge to `main`. Read the [Release procedure](release-and-hosts.md#release-procedure) before release work. |

**Host note (this VPS, wikiclaw-1):** headless Chrome + Xvfb and the `chrome-devtools` MCP server are available on this host (confirmed 2026-08-31) — do the `indexa serve` → `http://localhost:7620` visual confirmation above directly from here via a real browser-preview/screenshot, rather than deferring it to the Mac. The only things genuinely unavailable on this VPS are Xcode, iOS/Android simulators, and native mobile builds — none of which apply to indexa (the desktop app is Tauri, with no iOS/Android target).

## Markdown links

For Markdown edits, run the existing [docs workflow](../../.github/workflows/docs.yml) check locally when lychee is available:

```bash
lychee --offline --include-fragments --no-progress "**/*.md"
```

When relocating guidance, rebase relative links and heading fragments from the destination document, including incoming links. If lychee is unavailable, report that acceptance check as blocked rather than claiming parity with CI.

## Shared files and counters

**MCP/UI tool-count tokens are hot files.** [.orchestration/lanes.yml](../../.orchestration/lanes.yml)'s `hot_files` list flags `crates/mcp/src/lib.rs`, `crates/web/src/lib.rs`, `AGENTS.md`, `README.md`, `USAGE.md`, `docs/how-to/live-retrieval-over-mcp.md`, `CHANGELOG.md`, `crates/cli/src/lib.rs`, and `apps/indexa/src/main.rs` as pure-addition edit targets shared by multiple lanes — expect a trivial rebase there, not a logic conflict. Never hand-edit a tool-count number in prose without also updating the code: `lanes.yml`'s `counters` are `mcp_tool_count: 53`, `ui_js_fragment_count: 32`, `ui_css_fragment_count: 23`; the pinned `doc_tool_count_matches_code` test enforces the MCP one (see [Feature surface](operations.md#feature-surface-timeless--details-in-changelogmd)) and is exactly the kind of check that silently breaks if the doc and the code drift.
