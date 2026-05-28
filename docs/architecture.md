# Architecture

A map of how Indexa is structured — crates, data flows, and key design decisions.

---

## Crate map

```
apps/indexa          — CLI binary (main entry point)
│
├── crates/cli        — clap command definitions (Commands enum, arg types)
├── crates/core       — Config, Store (SQLite+FTS5+embeddings), surface scan, watcher
├── crates/parsers    — File type parsers (text, Markdown, code, PDF, EPUB, Org, office, image, media)
├── crates/embed      — Embedding adapter trait + Ollama/OpenAI/Google/llama.cpp impls
├── crates/llm        — LLM adapter trait + Ollama/OpenAI/Anthropic/llama.cpp impls
├── crates/query      — Hybrid search (FTS5 + vector) and RAG answer synthesis
└── crates/web        — Axum HTTP server + embedded single-page UI
```

Dependency direction: `apps/indexa` depends on all crates. `crates/query` depends on `core`, `embed`, and `llm`. `crates/web` depends on `core`, `embed`, `llm`, and `query`. No circular dependencies.

---

## Data flow — `indexa deep`

```
1. Walk          apps/indexa::cmd_deep
                 └─ walkdir over target path, respecting surface hints (Skip/StructureOnly)

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

```
1. Embed query   crates/embed::Embedder::embed(question)
                 → query_vec: Vec<f32>

2. Hybrid search crates/core::store::Store::hybrid_search(
                     query_text, Some(&query_vec),
                     mode (Rrf|Sparse|Dense), scope, top_k, rrf_k
                 )
                 ┌─ Sparse branch: FTS5 BM25 query  → ranked by BM25 score
                 ├─ Dense branch:  brute-force cosine scan → ranked by similarity
                 └─ RRF fusion: score = Σ 1/(k + rank_i), k=60 default
                 → Vec<SearchHit> { chunk_id, entry_path, heading, snippet, score }

3. Synthesize    crates/query::synthesize_from_hits(hits, llm, question, &qa_cfg)
                 └─ builds a context window from top chunks (budget: ~4000 words)
                    sends to LLM (Ollama gemma2:9b default, or configured model)
                 → Answer { answer: String, sources: Vec<SourceCitation> }
```

Files: [`crates/query/src/qa.rs`](../crates/query/src/qa.rs) · [`crates/core/src/store.rs`](../crates/core/src/store.rs) · [`crates/llm/src/lib.rs`](../crates/llm/src/lib.rs)

---

## Storage

Single-file SQLite at the platform data directory:

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/indexa/index.db` |
| Linux | `~/.local/share/indexa/index.db` (XDG) |
| Windows | `%APPDATA%\indexa\index.db` |

Three tables:
- **`entries`** — one row per file: `path`, `size`, `mtime`, `mime`, `category`
- **`chunks`** — one row per chunk: `entry_path` FK, `seq`, `heading`, `text`, `language`, `embedding` BLOB
- **`chunks_fts`** — FTS5 virtual table over `chunks(text, heading)` for BM25 full-text search

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
| `ollama` | `OllamaLlm` | `OLLAMA_HOST` | `gemma2:9b` |
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

`indexa watch <path>` uses `notify` (cross-platform inotify/FSEvents/ReadDirectoryChanges) to detect `Create` / `Modify` / `Remove` events. On each event the affected file is re-parsed and re-embedded; removed files are deleted from the store. The watcher uses a 500ms debounce window to coalesce rapid saves.

Source: [`crates/core/src/watcher.rs`](../crates/core/src/watcher.rs)
