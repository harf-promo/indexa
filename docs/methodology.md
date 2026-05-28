# Indexa — Context Construction Methodology

This document explains the technical decisions behind how Indexa builds context, generates embeddings, and retrieves answers. Every default described here is overridable via `config.toml` (see [Config Reference](config.md)).

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

### Why not re-ranking?

Cross-encoder re-rankers (e.g. BGE-reranker-v2-m3) add 100–500ms per query and require a second model. They improve recall at higher `top_k` values but are not worth the latency for `top_k=8`. Available as opt-in via `rerank = true`.

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

## What's opt-in (not default)

| Feature | Why opt-in | How to enable |
|---|---|---|
| **Whisper transcription** (audio) | Requires a ~150MB model + compute | `[parsers.audio] transcribe = true` |
| **Vision captioning** (images) | Requires a vision model | `[parsers.image] caption = true` |
| **OCR** (scanned PDFs) | Requires Marker or Tesseract CLI | `[parsers.pdf] backend = "marker"` |
| **Re-ranking** | Adds latency, requires second model | `[retrieval] rerank = true` |
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
