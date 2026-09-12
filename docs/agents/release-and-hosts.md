Read this before version bumps, tagging, publishing, desktop builds, signing, or deciding which host can verify a change.

# Indexa release and host boundaries

Run commands from the repository root. The root [Git & PR flow](../../AGENTS.md#git--pr-flow) and [Guardrails](../../AGENTS.md#guardrails) always apply.

## Release procedure

1. `git checkout -b bump-X.Y.Z`; bump `version` in BOTH root `Cargo.toml` and `apps/indexa-desktop/Cargo.toml`
2. `git commit -s -m "chore: bump version to X.Y.Z"` → PR → squash-merge on green
3. `git checkout main && git pull && git tag vX.Y.Z && git push origin vX.Y.Z`
4. Release CI builds 5 binary targets + Apple Silicon `.dmg` (Developer ID signed + notarized when Apple secrets present — [docs/signing.md](../signing.md)).

There is no merge-triggered deploy: [.orchestration/lanes.yml](../../.orchestration/lanes.yml)'s `deploy_on_merge` is empty, and [release.yml](../../.github/workflows/release.yml) triggers only on `v*.*.*` tag pushes.

## Host boundaries — VPS vs Mac

Nothing in this repo is Mac-only — `mac_only_paths: []` in fleet facts, and indexa has no `ios/` directory, no Xcode project, and no Expo/EAS step anywhere. The Tauri desktop app (`apps/indexa-desktop`) is cross-platform: its 5 release binary targets — plus the Apple Silicon `.dmg` (Developer ID signed + notarized, [docs/signing.md](../signing.md)) — are built by `release.yml` on GitHub-hosted multi-OS runners, not on this VPS, so nothing about that pipeline needs to run here. A *local* desktop build attempted on this box (`cargo build --manifest-path apps/indexa-desktop/Cargo.toml`) needs Tauri's Linux system deps (webkit2gtk etc. — the same reason it's excluded from `cargo build --workspace` on CI, per [Operational facts](operations.md#operational-facts)); that's a Linux-packaging concern, not a Mac-only one.

Web UI verification is a VPS strength here, not a gap: headless Chrome + Xvfb and the `chrome-devtools` MCP server are available on this host (confirmed 2026-08-31) — browser-preview and screenshot checks for the `:7620` web UI can and should happen from this VPS directly (see the host note under [Verification before done](verification.md#verification-before-done)). The only things genuinely unavailable on this host are Xcode, iOS/Android simulators, and native mobile builds — irrelevant to indexa, which ships a desktop app, not a mobile one.
