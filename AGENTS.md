# Indexa — agent contract

Feature history lives in `CHANGELOG.md` — do not narrate versions here. This file holds only the pitch, the invariants, and the procedures.

## Canonical pitch

Indexa is **the local context engine for AI**. The index is the substrate; context is the product. Never revert to "file indexer" framing in user-facing copy. Two audiences, one engine: it saves **cloud** AI tools their paid token budget, *and* gives **local** models the context they can't hold in a small window — by serving a retrieved slice instead of the whole repo. **Context Packs** = subject-scoped, named, exportable bundles (XML/Markdown, never HTML). The name stays **Indexa**; the tagline carries "context" (see `memory/feedback_naming_decision.md`, `memory/feedback_positioning.md`).

## Local models required

```bash
ollama pull nomic-embed-text   # embedding (~270 MB)
ollama pull gemma3:4b          # file summaries (~2.5 GB)
ollama pull gemma3:12b         # dir roll-ups + Q&A (~8 GB)
```

## Feature surface (timeless — details in CHANGELOG.md)

- **MCP server:** 47 tools across router modules in `crates/mcp` composed in `tool_router()` (NOT one lib.rs), + 4 resources (`indexa://…`) + 3 prompts.
- **Retrieval:** hybrid BM25/FTS5 + dense embeddings, RRF fusion, archive/code-intent/recency boosts, rerank, MMR; eval-gated via `indexa eval` over `fixtures/self-golden.json`.
- **Ask:** grounded RAG; `synthesize:false` returns the raw slice; conversational via `session_id`; `explain_retrieval` traces scoring.
- **Context Packs** (create/add/remove/export/search, remote `add-url` opt-in; exports secret-redacted) · **code graph** (deps/who_imports/who_calls/blast_radius; 8 languages, 1-hop, case-sensitive) · **decision-review ledger** · **classification + importance weights** · **savings/impact accounting** (≈4 bytes/token estimate).
- **Web UI** at :7620 · **Tauri desktop app** (in-app updater) · **CLI** (`index scan deep summarize … doctor eval`).
- **Parsers:** ~84 formats incl. Office, PDF (+opt-in OCR), EPUB, email, iWork, archives, opt-in multimodal.

## Load-bearing invariants — do not "fix" or remove

- **Web UI:** pure vanilla JS + SVG, zero frontend libraries. JS/CSS are `include_str!`-concatenated in `crates/web/src/lib.rs` — a new `NN-name.js`/`.css` MUST be added to that concat list or it is dead. Bundle contains emoji → `grep -a`. Syntax highlighter stays a client-side dependency-free tokenizer (tree-sitter-highlight conflicts with the parsers' tree-sitter 0.26).
- **Memory budget:** `resource::compute_budget` keys on `available_bytes`, NOT `total − used_memory()` (sysinfo counts the macOS compressor). Don't reintroduce the `micro_benchmark` dead field.
- **Retrieval boosts:** `retrieve()` in `crates/query/src/qa.rs` applies `apply_archive_penalty` (×0.15 on archive/archived/historical/deprecated/old segments) and `apply_code_intent_boost` (×1.6). Removing them makes answers cite `docs/archive/` and claim unshipped versions.
- **openssl-free tree:** all `reqwest` users pin `default-features = false, features = ["rustls-tls"]`; hf-hub pins `["ureq"]`. Verify: `cargo tree -i openssl-sys --target aarch64-unknown-linux-gnu` must be empty.
- **Verified non-bugs — don't "fix":** `trim_continuation` slice, `delete_subtree` prefix, redact count.
- **Web boot:** call the bare hoisted `restoreFromHash` from `08`'s boot, NOT `window.__indexaRestoreHash` (assigned later, in `26`).
- **`crates/web/src/update_control.rs`:** copy the value out before `send(None)` or it self-deadlocks. Update progress bridges Rust→web over SSE without Tauri IPC; `crates/update` stays web-agnostic (no circular dep).
- **Fingerprint matcher:** hand-written `*`/`?` glob — do NOT promote to `globset`; `**` rejected.
- **`directory_apps`:** persistence follows the classifications lifecycle; orphan-guard tests must include it in `orphan_rows_for`/`seed_full_entry`; app-detection runs as a SIBLING of `run_detectors`, not folded in.
- **Concurrency:** the qa crate takes conversation history as `&[PriorTurn]` by value so `&Store` never crosses `.await`.
- **CLI-skew detection:** `parse_plist_short_version` anchors the exact `<key>CFBundleShortVersionString</key>` key (loose "Version" grabs the wrong dict entry). doctor/status/MCP are authoritative; desktop marker + web banner secondary. Restart the MCP server after a CLI update.

## Verification before declaring done

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release
```

UI changes: `indexa serve` → visually confirm at http://localhost:7620 (headless-CDP harness fallback: `memory/feedback_browser_verification.md`).

## Git workflow

Public repo in `harf-promo`; branch protection on `main` (PR + green CI: fmt/clippy/test on 3 OSes, license check, DCO). **Never push directly to main.**
1. `git checkout -b <short-feature-name>`
2. `git commit -s` (DCO Signed-off-by required on every commit)
3. Push → PR → squash-merge on green. Missing sign-offs: `git rebase --signoff origin/main` + `git push --force-with-lease`.

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
