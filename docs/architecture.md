# Architecture

A map of how Indexa is structured — crates, data flows, and key design decisions.

---

## Crate map

```
apps/indexa          — CLI binary (main entry point)
│
├── crates/cli        — clap command definitions (Commands enum, arg types)
├── crates/core       — Config, Store (SQLite+FTS5+embeddings), surface scan, watcher, resource engine
├── crates/parsers    — File type parsers (text, Markdown, code, PDF, EPUB, Org, office, image, media)
├── crates/embed      — Embedding adapter trait + Ollama/OpenAI/Google/llama.cpp impls
├── crates/llm        — LLM adapter trait + Ollama/OpenAI/Anthropic/llama.cpp impls
├── crates/query      — Hybrid search (FTS5 + vector), reranking, and the unified RAG answer() pipeline
├── crates/web        — Axum HTTP server + embedded single-page UI (live SSE jobs)
└── crates/mcp        — stdio Model Context Protocol server (`indexa mcp`) exposing the index to AI agents
```

Dependency direction: `apps/indexa` depends on all crates. `crates/query` depends on `core`, `embed`, and `llm`. `crates/web` and `crates/mcp` depend on `core`, `embed`, `llm`, and `query`. No circular dependencies.

All three query surfaces — CLI `ask`, web `/api/ask`, and the MCP `ask` tool — call the single
Send-safe `query::answer(db_path, …)` entry point, so retrieval, optional reranking, and the
empty-result short-circuit behave identically everywhere.

Because the MCP server returns a small retrieved slice on demand, a **local** agent can offload its
context to disk — querying `get_summary` / `search` instead of holding the whole repo in its
window. See [Why an external context store helps local models](methodology.md#why-an-external-context-store-helps-local-models).

---

## Data flow — `indexa deep`

```
1. Walk          apps/indexa::cmd_deep
                 └─ parallel jwalk over target path; Skip-classified dirs (target/, node_modules/,
                    .git/, caches) are pruned at read-dir time and never descended into

2. Parse         crates/parsers::registry::parse(path)
                 └─ dispatches by extension → correct Parser impl
                    TextParser / MarkdownParser / CodeParser (tree-sitter) /
                    PdfParser / EpubParser / OrgParser / OfficeParser /
                    ImageParser / MediaParser
                 → Vec<Chunk> { heading, text, language, seq }

3. Embed         crates/embed::Embedder::embed(chunk.text)
                 └─ OllamaEmbedder (default: nomic-embed-text, 768 dim)
                    or OpenAIEmbedder / GoogleEmbedder / local llama.cpp
                 → Vec<f32>  (stored as raw LE bytes in SQLite BLOB)

4. Store         crates/core::store::Store::upsert_entry / upsert_chunk
                 └─ SQLite: entries table + chunks table + chunks_fts FTS5 virtual table
                 → persisted to platform data dir (~/.local/share/indexa/index.db on Linux)
```

Files: [`apps/indexa/src/main.rs`](../apps/indexa/src/main.rs) · [`crates/parsers/src/registry.rs`](../crates/parsers/src/registry.rs) · [`crates/core/src/store.rs`](../crates/core/src/store.rs)

---

## Data flow — `indexa ask`

The entry point is `crates/query::answer(db_path, embedder, llm, question, &cfg)`. It is
**Send-safe**: the `&Store` is opened in a synchronous inner scope and dropped before any `.await`,
so the future is `Send` (required by the axum web server and the rmcp MCP server).

```
1. Embed query   crates/embed::Embedder::embed(question)        [skipped for sparse-only mode]
                 → query_vec: Vec<f32>

2. Retrieve      crates/query::retrieve(&store, …)  (sync scope — store dropped before any await)
                 └─ Store::hybrid_search(query_text, Some(&query_vec), mode (Rrf|Sparse|Dense), scope, top_k, rrf_k)
                    ┌─ Sparse branch: FTS5 BM25 query  → ranked by BM25 score
                    ├─ Dense branch:  brute-force cosine scan → ranked by similarity
                    └─ RRF fusion: score = Σ 1/(k + rank_i), k=60 default
                    + optional parent-summary boost (summary_weight)
                 → Vec<SearchHit> { chunk_id, entry_path, heading, text, rrf_score }

   (empty hits → short-circuit with a "run `indexa deep`/`summarize` first" message — no LLM call)

3. Rerank        crates/query::apply_rerank(LlmReranker, …)     [only if cfg.rerank = true]
                 └─ one listwise local-model call reorders candidates; FAILS OPEN (any error/empty
                    /unparseable output → original order, so it can never make `ask` worse)

4. Synthesize    crates/query::synthesize_from_hits(hits, llm, question, &cfg)
                 └─ builds a context window from top chunks (default budget: 4000 chars)
                    sends to the LLM (Ollama gemma3:12b default, or configured model)
                 → Answer { answer: String, sources: Vec<SourceCitation> }
```

Files: [`crates/query/src/qa.rs`](../crates/query/src/qa.rs) · [`crates/query/src/rerank.rs`](../crates/query/src/rerank.rs) · [`crates/core/src/store.rs`](../crates/core/src/store.rs) · [`crates/llm/src/lib.rs`](../crates/llm/src/lib.rs)

---

## Storage

Single-file SQLite at the platform data directory:

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/dev.indexa.Indexa/index.db` |
| Linux | `~/.local/share/indexa/index.db` (XDG) |
| Windows | `%LOCALAPPDATA%\indexa\Indexa\data\index.db` |

Core tables:
- **`entries`** — one row per file: `path`, `size`, `mtime`, `mime`, `category`
- **`chunks`** — one row per chunk: `entry_path` FK, `seq`, `heading`, `text`, `language`, `embedding` BLOB
- **`chunks_fts`** — FTS5 virtual table over `chunks(text, heading)` for BM25 full-text search
- **`summaries`** — hierarchical per-node summaries with tiered L0 (abstract) / L1 (full) text and an optional summary embedding
- **`summary_queue`** — background summarization work queue (`pending` / `in_flight` / `done` / `failed`)

Vector search is brute-force cosine scan over the `embedding` BLOBs — fast enough for <300K chunks (typically under 200ms on a laptop).

---

## Adapters

### Embedding

| Provider key | Struct | Env var | Default model |
|---|---|---|---|
| `ollama` | `OllamaEmbedder` | `OLLAMA_HOST` | `nomic-embed-text` |
| `openai` | `OpenAIEmbedder` | `OPENAI_API_KEY`, `OPENAI_BASE_URL` | `text-embedding-3-small` |
| `google` | `GoogleEmbedder` | `GOOGLE_API_KEY`, `GOOGLE_BASE_URL` | `text-embedding-004` |
| `llamacpp` | `OpenAIEmbedder` (compat) | `OPENAI_BASE_URL` | configurable |

### LLM (answer synthesis)

| Provider key | Struct | Env var | Default model |
|---|---|---|---|
| `ollama` | `OllamaLlm` | `OLLAMA_HOST` | `gemma3:12b` |
| `openai` | `OpenAILlm` | `OPENAI_API_KEY`, `OPENAI_BASE_URL` | `gpt-4o-mini` |
| `anthropic` | `AnthropicLlm` | `ANTHROPIC_API_KEY` | `claude-haiku-4-5-20251001` |
| `llamacpp` | `OpenAILlm` (compat) | `OPENAI_BASE_URL` | configurable |

URL resolution order for all adapters: config `base_url` → env var → compiled-in default.

---

## Surface scan

`indexa scan` runs in two modes:
- **`--all`** — traverses from filesystem root using a registry of `PathHint` predicates that classify directories into categories (`documents`, `code`, `media`, `cache`, `system`, etc.) and assign a `DeepScanPolicy` (`Index`, `StructureOnly`, or `Skip`).
- **Path arguments** — applies the same predicates to the specified paths.

Virtual filesystems (`/proc`, `/sys`, `/dev`), caches (`~/.cache`, `node_modules`), and build artifacts are classified as `Skip` so `indexa deep` never tries to index them.

Source: [`crates/core/src/surface.rs`](../crates/core/src/surface.rs)

---

## File watcher

`indexa watch <path>` uses `notify-debouncer-full` (over cross-platform `notify` — inotify/FSEvents/ReadDirectoryChanges) to detect `Create` / `Modify` / `Remove` events. The debouncer coalesces editor save bursts into a single re-index. On each event the affected file is re-parsed and re-embedded; removed files are deleted from the store.

Source: [`crates/core/src/watcher.rs`](../crates/core/src/watcher.rs)

---

## Where to add things (contributor map)

The workspace is a strict DAG — `core` at the bottom, surfaces at the top. New code almost always
slots into one of these seams:

| You want to… | Touch | Pattern to copy |
|---|---|---|
| Support a new file format | `crates/parsers/` | any existing parser + register it in `registry.rs` (or ship it out-of-tree via the Plugin SDK) |
| Add an embedding/LLM provider | `crates/embed/` / `crates/llm/` | the Ollama adapter; keep `reqwest` on `rustls-tls` (cross-compile invariant) |
| Add a store table or query | `crates/core/src/store/` | one file per concern (`weights.rs`, `classify.rs`, …); DDL + migration in `schema.rs` (manual `sqlite_master` detection + IMMEDIATE tx — no `user_version`); invariant tests in `store/tests.rs` |
| Add a CLI command | `apps/indexa/src/commands/` | one file per command, wire in `main.rs` |
| Add a web endpoint | `crates/web/src/handlers/` | one handler module per feature; **new JS/CSS files must be appended to the `include_str!` concat in `crates/web/src/lib.rs` or they are silently dead** |
| Add an MCP tool | `crates/mcp/src/lib.rs` | `#[tool(description = …)]` method; each tool opens its own `Store`; update the tool-count in README/CLAUDE.md (CI checks it) |

Two store rules that are easy to violate accidentally:

- **No FK cascades.** Referential integrity is manual — entry deletion cleans up chunks / FTS /
  edges / summaries / queue / classifications in `entries.rs`. `importance_weights` is *deliberately
  exempt* (standing user intent survives re-indexing). A new child table must choose a side and add
  a test either way.
- **`chunks` IDs are AUTOINCREMENT and never reused** — the in-memory ANN index keys on them.

Verification gate for every PR: `cargo fmt --check && cargo clippy --workspace -- -D warnings &&
cargo test --workspace`. The desktop app builds separately
(`cargo build --manifest-path apps/indexa-desktop/Cargo.toml`) — it is excluded from `--workspace`.
