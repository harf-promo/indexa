Read this before changing the SQLite schema or summary queue, the resource watchdog, the desktop updater or release workflow, web config tests, or merging dependency PRs.

# Indexa gotchas and traps

Code paths are relative to the repository root. The always-loaded rules remain in [AGENTS.md](../../AGENTS.md); version history lives in [CHANGELOG.md](../../CHANGELOG.md). Each item below was re-checked against the code on 2026-10-05.

## Schema and store

- **Bump `SCHEMA_VERSION`** (`crates/core/src/store/schema.rs`) whenever the DDL or any migration in `init_schema` changes. A DB stamped at the old value skips the new migration and silently misses the column or table.
- **Never index a migration-added column in the base DDL.** If a column arrives via a later `ALTER TABLE ... ADD COLUMN`, create its index (`CREATE INDEX IF NOT EXISTS`) inside the migration block, after the `ALTER`. On an upgraded DB the base `CREATE TABLE IF NOT EXISTS` is a no-op, so a base-DDL index on the new column runs first, fails with `no such column`, and `Store::open` aborts every CLI/web/MCP command (the v0.69.0 release-breaker, `tool_usage.session_id`). Fresh and in-memory test DBs get the column from the base table and cannot catch this; add a test that builds the pre-migration table shape on a temp-file DB and calls `Store::open`, like `opens_pre_v069_index_missing_tool_usage_session_id` in `crates/core/src/store/tests/usage.rs`.
- **Chunk ids are `AUTOINCREMENT`** so they are never reused after a delete: the HNSW ANN index maps a node back to a chunk by id, and a reused id would mis-attribute results. A one-time migration enforces it on open; do not remove it.
- **No hard foreign key from chunks/summaries to entries.** `deep` and `summarize` legitimately run without `scan`, so entry-less chunks and summaries are allowed. Integrity is manual cleanup in `entries.rs`, locked by the `delete_entry_leaves_no_orphans` and `delete_subtree_leaves_no_orphans` tests (`crates/core/src/store/tests/entry_cleanup.rs`). The summary-queue self-clean (orphan drain, `queue_stats`, enqueue skip, `prune`) and `prune_orphans` all bypass when the index has zero entries; never treat "no entries" as "everything is an orphan".
- **Defer-rollup:** a directory waits for all its children before rolling up, capped at `MAX_DIR_DEFERS` (1200, `crates/query/src/lib.rs`). If summarization stalls, look for stuck `in_flight` children.
- **Sparse retrieval:** `build_fts_query` (`crates/core/src/store/search.rs`) tokenizes, drops stopwords and 1-character tokens, and ORs the terms after the exact phrase. It also feeds the lexical arm of RRF, so changing it affects hybrid `ask` and `search`. There is no stemming (`reindex` does not match `re-index`).
- **Hermetic indexing for CI:** plain `deep` needs Ollama when the embed provider is ollama. Use `indexa deep --no-embed` (FTS only, no model calls); a later plain `deep` re-embeds the vector-less chunks.
- **Equivalence-oracle tests for risky rewrites.** When replacing an obviously-correct query or implementation with a faster one, keep the old one as a `#[cfg(test)] fn *_reference` and assert byte-identical output on a rich seeded fixture. `tree_level_reference` in `crates/core/src/store/search.rs` is the model.

## Resource watchdog

Memory is sensed server-side through `sysinfo` in `crates/core/src/resource.rs`, so a native or Tauri shell does not improve it.

- `compute_budget` is `min(available_bytes, gpu_wired_limit) - headroom`, with `total - used` only as a fallback when `available_memory()` reports 0. Never derive available RAM from `total - used_memory()` on macOS: on sysinfo 0.39 `used_memory()` includes the compressor (10+ GB on a busy Mac) and produced false "out of memory" refusals. The RAM gauge may still show `used_memory()` because that matches Activity Monitor. Regression test: `budget_keys_on_available_not_compressor_inflated_used`.
- `assess` judges pressure by budget, not by swap fraction (macOS swap is sticky and never drains): `budget > 0` is `Ok`, `-H/2 < budget <= 0` is `Throttle`, `budget <= -H/2` is `Critical`. Do not reintroduce a swap-fraction trigger.
- The pause loop ticks 5 s (Critical) or 2 s (Throttle), caps at `MAX_PAUSE_SECS` (300) and then proceeds. Resume gates on the budget recovering, and resident models are unloaded once on a Critical entry so RAM can actually recover (`crates/query/src/worker.rs`, `crates/web/src/jobs_exec/watchdog.rs`).
- `num_ctx` must be sent to Ollama (`DEFAULT_NUM_CTX = 4096`, `crates/embed/src/ollama.rs`), otherwise the model loads at its 32k default window.
- Before bumping `sysinfo`, re-read `crates/core/src/resource.rs`: `System::new_with_specifics` changed in 0.39.

## Desktop updater and release

- **Two update paths exist; only one is safe.** The Tauri native updater (menu-bar "Check for Updates...") swaps the whole notarized bundle. The CLI binary self-replace (`indexa_update::apply`) must never run against the desktop `.app`: it would swap the headless CLI binary over `Contents/MacOS/indexa-desktop` and strip the Developer ID signature, after which Gatekeeper rejects the quarantined bundle. Three independent guards enforce this: the desktop does not set `INDEXA_WEB_ALLOW_UPDATE`, `crates/update` refuses when `INDEXA_DESKTOP=1` or when `current_exe()` sits inside `Contents/MacOS/` of a `.app` (`self_replace_refusal`), and the web apply endpoint returns 403 on desktop. Keep all three.
- **Never ad-hoc re-sign (`codesign --force --sign -`) a Developer ID + notarized bundle.** It strips notarization. `resign_app_bundle` in `apps/indexa-desktop/src/main.rs` fails closed: it only re-signs a bundle it can positively confirm is ad-hoc or linker-signed. The CLI path must re-sign after `self_replace` on macOS 26+ or the Code Signing Monitor kills the binary (exit 137).
- **Recovering a bricked desktop app:** reinstall from the notarized DMG on the release. Index data lives in `~/Library/Application Support/dev.indexa.Indexa/`, not in the bundle. Check with `codesign -dv` (Developer ID), `spctl -a -t exec` (Notarized) and `xcrun stapler validate`.
- **GitHub Actions forbids the `secrets` context in a step-level or job-level `if:`**; it fails the whole workflow at parse time with zero artifacts, and local YAML validation does not catch it. Pass the secret through `env:` and test it in the run script, as `release.yml` does for the signing keys.
- **Release builds are Developer ID signed and notarized** when the Apple secrets exist; see [signing](../signing.md). A stale dev-build `apps/indexa-desktop/target/release/bundle/macos/Indexa.app` can appear as a second Spotlight entry serving an old web bundle; delete the build artifact rather than debugging the app.

## Tests that touch config

Web config-write handlers read and save through `AppState.config_path` (`crates/web/src/lib.rs`), which the test constructor points at a unique scratch file, so a `#[tokio::test]` can safely reach `config::save`. Before that seam existed, a test silently overwrote the developer's live `config.toml` on every `cargo test --workspace`. Rules that remain:

- Build the state with `state_with` / `state_with_db`, never with a hand-rolled `AppState` that uses `default_config_path()`.
- Post an explicit canary value (`/nonexistent/<test-name>-canary-do-not-reuse`), never a plausible real binary path or key shape.
- A new gate test must prove the env gate runs before any `config::load` or `save`. The existing model is `api_config_resource_set_writes_the_scratch_path_never_the_real_config`.

## Dependency PRs and CI

- Branch protection on `main` (read with `gh api repos/harf-promo/indexa/branches/main/protection`, 2026-10-05): `strict: true` (the branch must be up to date), no required reviews, force-push and deletion blocked, and these required contexts: `fmt + clippy + test` on ubuntu, macos and windows, `License and advisory check`, `DCO sign-off check`, `desktop build (macOS)`, `web smoke (headless Chrome)`, `retrieval eval (self-golden, hermetic)`. No repository rulesets exist.
- Because of `strict`, several PRs that each regenerate the desktop lockfile must merge serially, each branched from the just-merged `main`.
- Dependabot edits only `Cargo.toml`, so the tracked `apps/indexa-desktop/Cargo.lock` goes stale and only `desktop build (macOS)` fails. Fix on the branch with `cargo update --manifest-path apps/indexa-desktop/Cargo.toml -p <dep>` (use `-p dep@oldver` when several majors coexist) and confirm with `cargo metadata --manifest-path apps/indexa-desktop/Cargo.toml --locked >/dev/null`. See also [verification](verification.md#verification-before-done).
- Bump re-export-coupled dependencies in the same PR: `notify` and `notify-debouncer-full` (the debouncer re-exports notify's `Event` and `EventKind`). Bumping one alone leaves two notify majors in the tree and core CI fails on a type mismatch.
- Windows CI occasionally fails a crates.io download (`failed to get <crate>; download of ... failed; curl failed`). It is not a code bug: `gh run rerun <run-id> --failed`. Confirm green with `gh pr checks <pr>` before merging; do not trust the exit status of `gh run watch` alone.

## Shipping traps

- Never write a squash-merge hash into docs before the merge: the squash mints a new hash. Merge, sync `main`, then record `git rev-parse --short HEAD`.
- A green local `cargo fmt --all --check` can still fail CI: CI installs the current stable rustfmt through `dtolnay/rust-toolchain@stable`, which can be newer than your local one. Wrap multi-element `vec![]` and builder literals near 90 columns one item per line, and read the exact diff from `gh run view <id> --log-failed`.
- `cmd | tail; echo $?` reports the pipe's exit status, not `cmd`'s. Assert the exit code of the command itself.
- After splitting or moving a Rust module, run `RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc -p <crate> --no-deps`; `cargo doc` is not one of the CI gates, so bare intra-doc links to now-sibling items rot silently. Qualify them (`super::`, `crate::`).
- When adding a CHANGELOG entry by edit, do not consume the previous `## [X.Y.Z]` header below `## [Unreleased]`. Afterwards run `grep -nE '^## \[[0-9]' CHANGELOG.md | head` and check the top versions are contiguous and descending. The release `changelog-gate` only checks the current tag's section, not the chain.
- Re-verify every finding from a multi-agent audit against the code before implementing it; roughly one in ten was a false positive that would have caused a regression.
- After post-review edits, re-stage and run `git status` before committing; confirm a background commit landed with `git log --oneline -1` before pushing.
