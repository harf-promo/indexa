# Indexa — Context Construction Methodology

This document explains the technical decisions behind how Indexa builds context, generates embeddings, and retrieves answers. Every default described here is overridable via `config.toml` (see [Config Reference](config.md)).

**In one sentence:** Indexa reads your files once, turns each piece into a searchable summary, and at question time hands your AI tool only the handful of pieces that matter — so the model reasons over a small, relevant slice instead of your whole disk. The detail below is *how* it keeps that slice relevant, and the honest trade-offs of doing it this way.

---

## Overview

Indexa builds context in two phases:

```
Phase 1 — Surface scan (fast):
  Walk directory tree → classify paths → store structure
  Output: labeled regions, no AI calls, <60s on large disks

Phase 2 — Deep scan (per-region, background or on-demand):
  Parse file content → chunk → embed → index
  Output: searchable semantic index
```

---

## Why an external context store helps local models

Local models (Ollama, llama.cpp) live under a hard context budget: the attention KV-cache grows with every token in the window and competes with the model's weights for VRAM, native context windows are small (often 4–8K tokens), and prefill cost scales roughly quadratically with context length. Stuffing a whole repo into the prompt is therefore slow, memory-hungry, and frequently impossible on consumer hardware.

Indexa shifts that burden off the model. The hierarchical context graph lives on disk; at query time, retrieval hands the model only a small, ranked slice (default ~4000-char budget — see [Answer synthesis](#answer-synthesis-rag)). The model can reason over a 4K window even when the underlying store covers hundreds of gigabytes.

**Honest trade-offs:**

- Indexa **sidesteps** the KV-cache problem by feeding small slices; it does **not** compress or quantize the cache itself — that optimization (PagedAttention, KV quantization) happens inside the inference engine independently.
- Retrieval can **miss** context if ranking is poor. Hybrid search (BM25 + vector) and optional reranking mitigate this, but it is not zero-risk: a model can only reason over what retrieval surfaces.
- The trade is **memory for latency** — you swap a large KV-cache for an embedding search plus a synthesis round-trip (typically 100–500 ms). Worth it for large stores, roughly neutral for a single small file.
- It helps only agents that **use** retrieval. An agent hard-coded to read whole files into its window gains nothing from an external store.

---

## File parsing

Each file type has a dedicated parser. Parsers are tried in order:

| Parser | Extensions | Notes |
|---|---|---|
| Code | `.rs .py .js .ts .tsx .go .java .mjs .cts .mts` | tree-sitter AST chunking |
| PDF | `.pdf` | pdf-extract (text-layer); scanned PDFs produce minimal output |
| Image | `.jpg .jpeg .png .webp .heic .tiff .cr2 .nef .arw .dng .bmp .gif` | EXIF metadata extraction |
| Media | `.mp3 .mp4 .m4a .flac .wav .ogg .opus .mkv .avi .mov .webm .aiff` | ffprobe metadata (duration, tags) |
| Office | `.xlsx .xls .ods .csv .tsv .docx .odt .rtf` | calamine (spreadsheets), zip/XML strip (docx) |
| Markdown | `.md .mdx` | heading-based structure chunker |
| Text | `.txt .log .conf .yaml .yml .json .toml .xml .html .css` | fixed-window chunker |

For files not matched by extension, MIME type detection is used as a fallback. Plain `text/*` files fall through to the text parser.

---

## Chunking

**Goal:** produce pieces of text that are semantically coherent and fit within the embedding model's context window.

### Structure-aware chunking (default)

For each file type, boundaries are detected from the document's own structure:

- **Markdown/HTML**: split at ATX headings (`#`, `##`, etc.); heading breadcrumb is stored as the chunk heading (e.g. `Introduction > Background`)
- **Code**: split at top-level AST nodes (functions, classes, impl blocks); the function/class name is stored as the heading
- **PDF**: split at page boundaries; page number stored in heading
- **Office**: tab-delimited rows for spreadsheets; paragraphs for docx

When a structural chunk exceeds `size` words, it is sub-split with `overlap` word overlap.

### Fixed-window chunking

Slides a window of `size` words with `overlap` words of overlap. Used as the fallback when structure detection yields nothing (e.g. a long script-style Python file with no functions).

### Why not sentence or paragraph chunking?

Sentence-level chunking fragments ideas that span multiple sentences. Paragraph-level has no reliable signal across all file types. Structure-aware chunking (headings, AST) produces chunks that are semantically complete and match how humans think about document organization. Research (Anthropic 2024, Pinecone benchmarks) shows 10–30% better retrieval recall over naïve fixed windows.

---

## Embedding

### Default model: nomic-embed-text (768 dim)

[nomic-embed-text](https://huggingface.co/nomic-ai/nomic-embed-text-v1) is produced by Nomic AI (backed by NVIDIA), released under the Apache-2.0 license. It scores strongly on MTEB benchmarks for its size class and runs fully locally via Ollama.

```
ollama pull nomic-embed-text
```

### Why not a larger/proprietary model by default?

1. **Local-first principle**: Indexa should work offline, without API keys, out of the box.
2. **Cost**: embedding an entire disk at cloud rates would be expensive.
3. **Privacy**: file contents never leave your machine with the default config.
4. **Adequacy**: 768-dim embeddings from nomic-embed-text have near-identical recall to larger models for typical document retrieval tasks.

OpenAI and other cloud providers are supported as opt-in alternatives (see [Config Reference](config.md)).

---

## Index storage

### SQLite + FTS5 + custom vector storage

All index data lives in a single SQLite database (`~/.indexa/index.db` or platform equivalent).

| Table | Contents |
|---|---|
| `entries` | File metadata: path, size, mtime, MIME, surface-scan category |
| `chunks` | Parsed text chunks with optional embedding blob |
| `chunks_fts` | FTS5 virtual table for full-text BM25 search |

Embeddings are stored as little-endian `f32` byte blobs directly in the `chunks` table. No external vector database is needed.

**Why SQLite?** Zero ops: one file, easy to back up, easy to delete. Brute-force cosine similarity is fast enough for up to ~300K vectors on commodity hardware. An HNSW sidecar will be added when needed.

---

## Retrieval

### Hybrid search (default: RRF k=60)

Every query runs two searches in parallel:

1. **Sparse (FTS5 BM25)**: exact and near-exact keyword matching
2. **Dense (cosine similarity)**: semantic meaning matching

Results are fused using **Reciprocal Rank Fusion (RRF)**:

```
score(d) = 1/(k + rank_sparse(d)) + 1/(k + rank_dense(d))
```

with the default `k=60` (matches Elasticsearch, Weaviate, Vespa defaults). RRF needs no score calibration across the two systems and is robust across query types.

### Re-ranking (opt-in)

A dedicated cross-encoder (e.g. BGE-reranker-v2-m3) would add 100–500ms per query and require a
second model — overkill for the default `top_k=8`, so it is **off by default**. When `rerank = true`,
Indexa instead runs a lightweight **listwise re-ranker that reuses the local generation model** in a
single extra call (no second model, no new native dependency). It **fails open**: any model error,
empty, or unparseable output falls back to the original retrieval order, so re-ranking can never make
`ask` worse. A future ONNX/`fastembed` cross-encoder can slot in behind the same `CrossEncoder` trait
via a Cargo feature.

---

## Answer synthesis (RAG)

The Q&A pipeline:

1. **Embed query** — convert the question to a vector
2. **Hybrid search** — retrieve top-k chunks by RRF score
3. **Pack context** — format chunks into an LLM prompt, budget-limited to ~4000 characters
4. **Synthesize** — send to the LLM with citation instructions
5. **Return** — answer text + source citations

The prompt instructs the LLM to:
- Answer only from the provided context
- Cite sources by `[number]`
- Admit when the answer isn't in the context

### Context packing

Chunks are included in ranked order until the character budget is exhausted. The parent file path and heading are included with each chunk so the LLM can produce accurate citations.

---

## Code graph & centrality

The signature graph (v0.18) is a **file-to-file call graph**: an edge `A → B` means file `A` calls a symbol that file `B` defines. It is built by joining the `calls` and `defines` edges extracted at `deep` time on the **bare symbol name**. This is deliberately lightweight, and the limits are honest ones:

- **Bare-name, case-sensitive, 1-hop.** No type resolution, no scope/namespace analysis, no overload disambiguation. Two unrelated functions that share a name will be linked. Symbols defined in more than 25 files (common helpers like `new`/`get`) are dropped as noise.
- **Seven languages.** Rust, Python, JavaScript, TypeScript, Go, Java, C/C++ — wherever the parser emits call/define edges.

### PageRank centrality (v0.20)

Each node carries a **weighted PageRank** score, computed over the *displayed* graph (after the edge cap is applied — so on a truncated graph, centrality is relative to what's shown). Rank flows along edges caller → callee, so a file **called by** many — or by other central files — scores highest; this surfaces hub/library files. Edge weight (number of shared call→define symbols) biases the flow toward stronger relationships. The algorithm is a standard power iteration (damping 0.85, dangling-mass redistribution, L1 convergence); scores sum to ~1.0.

Centrality drives node **size** in the Map graph view and the ranked "most central files" list in `indexa graph` and the `code_graph` MCP tool. It inherits the bare-name-matching imprecision above, so it is an **approximate** importance signal — useful for "what should I read first," not an authoritative dependency analysis. It does **not** feed search/QA ranking (that remains RRF + summary/importance-weight boosts); wiring centrality into retrieval is a possible future extension.

---

## What's opt-in (not default)

| Feature | Why opt-in | How to enable |
|---|---|---|
| **Whisper transcription** (audio) | Requires a ~150MB model + compute | `[parsers.audio] transcribe = true` |
| **Vision captioning** (images) | Requires a vision model | `[parsers.image] caption = true` |
| **OCR** (scanned PDFs) | Requires Marker or Tesseract CLI | `[parsers.pdf] backend = "marker"` |
| **Re-ranking** | Adds one extra local-model call per query (fails open) | `[retrieval] rerank = true` |
| **Contextual retrieval** | LLM call per chunk at index time (expensive) | `[describer] contextual_retrieval = true` |
| **Cloud embeddings** | Requires API key, costs money | `[embedding] provider = "openai"` |
| **Cloud LLM** | Requires API key | `[describer] provider = "anthropic"` |

---

## Decision log

Changes to defaults are recorded here with rationale.

| Date | Decision | Rationale |
|---|---|---|
| 2025-05 | Default embedding: `nomic-embed-text` via Ollama | Apache-2.0, strong MTEB, zero ops, local-first |
| 2025-05 | PDF parser: `pdf-extract` (pure Rust) | No C++ build dep, sufficient for text-layer PDFs |
| 2025-05 | Vector storage: SQLite f32 blobs | Zero ops, single file, adequate for <300K vectors |
| 2025-05 | Fusion: RRF k=60 | Parameter-free, matches industry defaults, robust |
| 2025-05 | No re-ranker by default | Latency cost > recall benefit at top_k=8 |
| 2026-05-28 | Default describer: `gemma2:9b` (was `qwen2.5:14b`) | Google, Apache-2.0; user preference for non-Chinese-company defaults |
| 2026-05-28 | Default describer upgraded: `gemma3:12b` / `gemma3:4b` (was `gemma2:9b` / `gemma2:2b`) | Gemma 3 (March 2025) outperforms Gemma 2 at same parameter count; available on Ollama |
| 2026-05-28 | Added `google` embedding provider | Google `text-embedding-004` matches nomic-embed-text dim (768), state-of-the-art quality |
