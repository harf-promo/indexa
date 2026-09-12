Read this when configuring local models, changing features or operational defaults, indexing, or inspecting the index and summary queue.

# Indexa operations and feature surface

Commands and code paths below are relative to the repository root. The always-loaded rules remain in [AGENTS.md](../../AGENTS.md).

## Local models required

```bash
ollama pull nomic-embed-text   # embedding (~270 MB)
ollama pull gemma3:4b          # file summaries (~2.5 GB)
ollama pull gemma3:12b         # dir roll-ups + Q&A (~8 GB)
```

## Feature surface (timeless — details in CHANGELOG.md)

- **MCP server:** 56 tools across router modules in `crates/mcp` composed in `tool_router()` (NOT one lib.rs), + 4 resources (`indexa://…`) + 3 prompts. A pinned test (`doc_tool_count_matches_code`) keeps this number honest — update it when tools change.
- **Retrieval:** hybrid BM25/FTS5 + dense embeddings, RRF fusion, archive/code-intent/recency boosts, rerank, MMR; eval-gated via `indexa eval` over `fixtures/self-golden.json`.
- **Ask:** grounded RAG; `synthesize:false` returns the raw slice; conversational via `session_id`; `explain_retrieval` traces scoring.
- **Context Packs** (create/add/remove/export/search, remote `add-url` opt-in; exports secret-redacted) · **code graph** (deps/who_imports/who_calls/blast_radius; 8 languages, 1-hop, case-sensitive) · **decision-review ledger** (durable, patch-id-anchored notes via `record_decision`) · **durable memory** (typed claims — observed/stated/inferred/recalled/hypothesis — with confidence, source hash, bitemporal validity, operator-only verification, and an opt-in `[memory] retrieval` context block) · **summarize-pass drift** (stale/orphaned/uncovered) · **classification + importance weights** · **savings/impact accounting** (≈4 bytes/token estimate).
- **Web UI** at :7620 · **Tauri desktop app** (in-app updater) · **CLI** (`index scan deep summarize … doctor eval`).
- **Parsers:** ~84 formats incl. Office, PDF (+opt-in OCR), EPUB, email, iWork, archives, opt-in multimodal.

Version history belongs in [CHANGELOG.md](../../CHANGELOG.md). Before editing MCP/UI count claims, read [Shared files and counters](verification.md#shared-files-and-counters).

## Operational facts

- **Multi-pass defaults:** `--passes` = 2 first-time, 1 refresh, hard cap 3 (Self-Refine: gains saturate at pass 3).
- **Security:** `POST /api/keys` gated by `INDEXA_WEB_ALLOW_KEY_EDIT=1`; config file 0600; keys never logged. Changes to this endpoint require explicit confirmation under the root [Guardrails](../../AGENTS.md#guardrails).
- **Classification priority:** filename phf_map → extension phf_map → `hyperpolyglot::detect` → MIME fallback.
- **One-shot indexing:** `indexa index <path>` = scan → deep → summarize; use for first builds/full refreshes.
- **Desktop app:** excluded from `cargo --workspace` (webkit2gtk absent on Linux/Windows CI runners); build via `cargo build --manifest-path apps/indexa-desktop/Cargo.toml`. Standard CI compiles it separately on macOS with `--locked`; the release workflow handles bundling/signing and publication. Read [verification](verification.md#verification-before-done) before dependency edits and [Host boundaries](release-and-hosts.md#host-boundaries--vps-vs-mac) before a local desktop build.
- **Index DB (macOS):** `~/Library/Application Support/dev.indexa.Indexa/index.db` (other platforms: [USAGE.md §2](../../USAGE.md#2-the-mental-model)). Queue health: `sqlite3 "$HOME/Library/Application Support/dev.indexa.Indexa/index.db" "SELECT state, COUNT(*) FROM summary_queue GROUP BY state"`.
