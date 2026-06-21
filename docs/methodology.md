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
| Notebook | `.ipynb` | one chunk per cell; code cells language-tagged from the kernel; outputs skipped |
| PDF | `.pdf` | pdf-extract (text-layer); scanned PDFs OCR'd via pdftoppm + tesseract when `[parsers.pdf] backend = "ocr"` (opt-in, external tools) |
| EPUB | `.epub` | spine-order chapters, XHTML tags stripped |
| SVG | `.svg` | text / `<title>` / `<desc>` extracted; path geometry & CSS ignored (no OCR of outlined text) |
| Image | `.jpg .jpeg .png .webp .heic .tiff .cr2 .nef .arw .dng .bmp .gif` | EXIF metadata extraction |
| Media | `.mp3 .mp4 .m4a .flac .wav .ogg .opus .mkv .avi .mov .webm .aiff` | ffprobe metadata (duration, tags) |
| Presentation | `.pptx .ppsx` | one chunk per slide + speaker notes + deck-level chart & SmartArt text |
| iWork | `.pages .numbers .key` | text from the embedded preview PDF (rendered snapshot; not every cell/formula) |
| Office | `.xlsx .xls .ods .csv .tsv .docx .odt .rtf` | calamine (spreadsheets); zip/XML for docx; RTF control-word stripper (skips font/colour/style tables) |
| Email | `.eml .msg` | mail-parser (`.eml`: subject/from/to/date + body + attachment names); `.msg` Outlook OLE: subject + body from MAPI streams (cfb) |
| Archive | `.zip .tar .tar.gz .tgz` | lists entry names + sizes (shallow; entry content not extracted) |
| Binary | `.so .dylib .exe .o .wasm .jar` | symbol / export names (object + wasmparser; `.jar` class names) — no disassembly |
| HTML | `.html .htm .xhtml` | `<script>`/`<style>` stripped, HTML→Markdown (htmd), then heading-aware chunks |
| Markdown | `.md .mdx` | heading-based structure chunker; YAML frontmatter (`title`/`tags`/`date`/…) lifted into a leading chunk |
| Text | `.txt .log .conf .yaml .yml .json .toml .xml .css` | fixed-window chunker |

For files not matched by extension, MIME type detection is used as a fallback. Plain `text/*` files fall through to the text parser.

**Known parsing gaps (honest coverage).** Legacy `.ppt`/`.doc` (PowerPoint/Word OLE binary — quiet stub; Outlook `.msg` IS now extracted), recursive archive extraction (archives are listed, not recursed into), and text drawn as SVG paths are not parsed; media/binary files without captioning contribute only metadata, and PDF OCR needs external tools (poppler + tesseract) and is opt-in. Slide **chart and SmartArt text** IS now extracted (deck-level, v0.54); embedded OLE objects inside slides still aren't. "Understands every file" means *text and metadata from every common type, with the rest listed honestly* — not full semantic extraction of every format.

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

## Hierarchical summarization & roll-up

Indexa doesn't just embed chunks — it writes a **summary for every node** and composes them up the
directory tree. This is the hierarchical context graph referenced in
[Why an external context store helps local models](#why-an-external-context-store-helps-local-models).

**Three tiers, defined.** Every file *and* every directory carries:

- **L0** — a one-line abstract (the first sentence of L1), for cheap scanning.
- **L1** — the full 1–4 sentence summary.
- **L2** — the raw content (the chunks themselves; files only).

All three are independently retrievable: MCP `get_summary` takes `tier = l0 | l1 | l2`, and the web
tree and CLI surface the same.

**Two-phase, bottom-up.**

1. **File pass.** Each file with chunks is summarised by the local LLM from a content sample; the L1
   summary is embedded into the same vector space as chunks, so summaries participate in hybrid
   retrieval alongside raw content.
2. **Directory pass.** Directories are summarised deepest-first. A folder's summary is *composed from
   its children's summaries* — "speak about themes, not filenames" — never by re-reading the raw
   files. Each parent waits for its children, so the roll-up climbs folder → parent folder → root.

**Why bottom-up.** A parent stands on its children's summaries rather than the raw tree, which keeps
cost bounded and lets an unchanged subtree skip the LLM entirely on re-summarise (a Merkle-style
`source_hash` folds in every child's hash — see
[Freshness limits of incremental re-summarize](#freshness-limits-of-incremental-re-summarize)).

**How it answers broad questions.** When a question is broad ("what is this project?", "how is X
architected?"), the answer prompt is prefixed with a **project-overview block** — the root's L1 plus
its top child directories' L0s — so the model has the shape of the whole before it reads any chunk.

**Honest limits.** Summaries are LLM-generated and can be imprecise; the raw L2 is always one
drill-down away to verify. A file with no extractable text (an un-captioned image, an opaque binary)
contributes only its path + classification to its folder's roll-up — so "every folder has context"
is true *structurally*, but a folder of un-captioned media rolls up thin until captioning or
transcription is enabled (see [What's opt-in](#whats-opt-in-not-default)). The directory prompt also
samples a bounded number of children for the LLM call even though the freshness hash covers all of
them, so a parent summary may not name every child by content while still refreshing when membership
changes.

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

### Re-ranking (on by default since v0.44)

Re-ranking the `top_k` (default 12) hits before synthesis is **on by default** (`[retrieval] rerank
= true`), with two selectable backends (`rerank_backend`):

- **`"llm"` (default)** — a lightweight **listwise re-ranker that reuses the local generation model**
  in a single extra call. No second model, no download, no new native dependency.
- **`"cross-encoder"` (opt-in, shipped v0.43)** — a pointwise DeBERTa-v2 cross-encoder
  (`mixedbread-ai/mxbai-rerank-xsmall-v1`, ~85 MB, Apache-2.0, CPU-only) downloaded from HuggingFace
  on first use and cached at `~/.cache/huggingface/hub/`. Higher quality at ~100–500 ms/query and a
  one-time download — hence opt-in. Falls back to `"llm"` if the model can't be loaded.

Both **fail open**: any model error, empty, or unparseable output falls back to the original
retrieval order, so re-ranking can never make `ask` worse.

### Scoping a query (file-aware Ask)

`ask` accepts a `scope` (CLI `--scope`, MCP `ask {scope}`, and — since v0.27 — the web UI, which
auto-scopes to the file or folder you have selected and shows an "Asking about: …" chip). Scope is a
**path-prefix filter** applied in both retrieval arms (`entry_path LIKE '<scope>%'`), not a hard
directory boundary — so a folder scope `/src/auth` also matches a sibling `/src/authentication.rs`.
For a single file it is effectively exact. Scoping narrows retrieval to that subtree, which is what
makes "what is this file?" answer about *that* file rather than the whole index — but a single-file
scope can return few hits, so the web UI offers to widen to the parent folder rather than silently
falling back to a whole-index search.

### Excluding content-free stub chunks

Binary, image, and media files that yield no extractable text get a placeholder chunk
(`File: <name>`, `Image: <name>`, `Media file: <name>`). These carry no semantic content but embed
near generic phrasings like "what is this file?", so retrieval **excludes** them (a short-length +
known-prefix filter in the search SQL, guarded again before synthesis) — they can never surface as an
answer source. The honest consequence: **a binary or image file with no caption contributes nothing
searchable** beyond its path and [classification](#decision-log) — enable [Vision
captioning](#whats-opt-in-not-default) to make image *content* retrievable.

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

### Agentic retrieval (opt-in)

`indexa ask --agentic` (or MCP `agentic: true`) replaces the single retrieval with a bounded **iterative** one — a "self-ask" loop: search → show the model a compact digest of what's been found (file paths + headings, *not* the full context) and ask whether an important part of the question is still uncovered → if so, take one focused follow-up query and search again → synthesize from the merged, deduplicated context. This helps on **compositional** questions ("how does X work *and* where is Y?") whose pieces live in different files that a single query won't co-retrieve.

It is **off by default** because each hop adds an LLM "decide" call. The loop is bounded (`--max-steps`, 1–5, default 3) and **fails open**: the between-hop decision is parsed leniently, and an unparseable reply, a repeated query, or a hop that surfaces no new chunks all end the loop — so a model that won't emit the `SEARCH:`/`DONE` actions simply degrades to ordinary one-shot retrieval rather than erroring or looping. Every hop reuses the same scoped retrieval, so `--scope` and the importance/summary boosts apply on each one.

---

## Code graph & centrality

The signature graph (v0.18) is a **file-to-file call graph**: an edge `A → B` means file `A` calls a symbol that file `B` defines, built from the `calls`/`defines`/`imports` edges extracted at `deep` time. It is deliberately lightweight, and the limits are honest ones:

- **Name-based with scoped resolution, case-sensitive, 1-hop.** No type resolution, no overload disambiguation. Symbols defined in more than 25 files (common helpers like `new`/`get`) are dropped as noise.
- **Eight languages.** Rust, Python, JavaScript, TypeScript (and TSX), Go, Java, C, and C++ — wherever the parser emits call/define edges. For C/C++, calls resolve through `call_expression` (including C++ method and `ns::`-qualified calls) and imports through `#include` (`<system>` and `"local"` headers); `#include` path matching is lexical/loose, so cross-file resolution falls back to the labeled bare-name tier more often than for the others.

#### Scoped call resolution (v0.25)

Every graph query (`code_graph`, `who_calls`, `blast_radius`, `related_files`, the Map view) resolves
each `calls X` edge against X's definition sites **at query time** — edge recording is unchanged, so
existing indexes get the precision win without re-indexing. Definition sites are ranked in tier order;
the first tier that matches wins:

1. **same-file** — the caller defines `X` itself: the call binds to the local definition and links to
   that file *only*. An intra-file helper named like a popular symbol no longer fans out repo-wide
   (this was the largest class of false positives).
2. **same-dir** — definers sharing the caller's directory: only those are linked.
3. **import** — definers whose file path matches one of the caller's recorded import strings.
4. **bare** — the remaining fallback: every definer of the name (the pre-v0.25 behavior), **labeled**
   as bare. Only this tier carries the bare-name caveat; surfaces show it only when bare edges are
   actually present.

The tier-3 matcher is **heuristic import-string matching, not semantic analysis**. Exactly these
forms resolve:

| Language | Resolves | Does not resolve |
|---|---|---|
| JS/TS | relative specifiers `./x`, `../y/z` (joined to the caller's dir; usual extensions and `/index.*` tried) | bare/package specifiers (`react`, `@scope/pkg`), path aliases (`@/...`), re-exports, dynamic `import()` |
| Rust | `crate::a::b` → `<crate-src>/a/b.rs` or `a/b/mod.rs` (crate root = the caller's nearest `src/` ancestor); `super::a` (one dir up per extra `super`); a trailing item segment is also tried (`use crate::a::b::item`) | external-crate paths (`std::fs`, `other_crate::x` — a crate *name* doesn't map to a directory lexically), `self::`, use-**renames'** aliases, macro-generated items |
| Python | absolute dotted modules `a.b` → suffix `a/b.py` or `a/b/__init__.py` | `sys.path` manipulation, namespace-package tricks; relative imports are recorded without their leading dots and degrade to a broader suffix match |
| Go / Java | — (package paths / FQCNs don't map to files lexically) | everything — their calls still get same-file/same-dir/bare tiers |

Unresolvable import strings simply contribute nothing and the call falls through to the bare tier —
resolution can *narrow* an edge set, never invent edges, so PageRank and Map node sizing stay
comparable with earlier versions.

#### Strict mode (v0.20, redefined in v0.25)

`indexa graph --strict`, the `code_graph`/`blast_radius` MCP `strict: true` flag, and `/api/graph?strict=1`
now **drop the bare tier entirely**: only structurally-resolved edges (same-dir/import) remain.
(Before v0.25 strict was a unique-definition *name* filter; resolution supersedes it — a uniquely-named
symbol with no structural link is now treated as the unconfirmed match it is, and a multi-definition
symbol that *does* resolve via an import is kept.)

- `who_calls` takes a bare name with no definer to disambiguate against, so its *input* can't be
  scoped; instead it groups its callers by resolution tier and annotates the bare group with how many
  files define the name (`defines_count`).
- `blast_radius`'s direct-caller set is name-matched for the same reason; the transitive hop is
  resolved — a transitive caller is included only when its call resolves back to a direct caller
  (or on the labeled bare fallback, which `strict` disables).

### PageRank centrality (v0.20)

Each node carries a **weighted PageRank** score, computed over the *displayed* graph (after the edge cap is applied — so on a truncated graph, centrality is relative to what's shown). Rank flows along edges caller → callee, so a file **called by** many — or by other central files — scores highest; this surfaces hub/library files. Edge weight (number of shared call→define symbols) biases the flow toward stronger relationships. The algorithm is a standard power iteration (damping 0.85, dangling-mass redistribution, L1 convergence); scores sum to ~1.0.

Centrality drives node **size** in the Map graph view and the ranked "most central files" list in `indexa graph` and the `code_graph` MCP tool. Since v0.25 most edges are scope-resolved, but bare-tier edges still contribute, so it remains an **approximate** importance signal — useful for "what should I read first," not an authoritative dependency analysis. It does **not** feed search/QA ranking (that remains RRF + summary/importance-weight boosts); wiring centrality into retrieval is a possible future extension.

---

## What "tokens saved" means

`indexa status`, MCP `get_stats`, and the web header report an **estimated** token saving:

- **Served** is measured: the UTF-8 bytes of what retrieval actually returned (answers, summaries,
  snippets, capped file reads).
- **The counterfactual is an estimate, not a measured baseline**: the full on-disk size of every
  distinct file behind what was served — i.e. what a client would have read if it had opened those
  files whole instead of querying the index. A real client might have read fewer files, or more
  (re-reading across sessions); nothing claims otherwise.
- Tokens ≈ bytes / 4, labeled as such everywhere. Both quantities are bytes, so on
  multibyte-heavy text (CJK, Arabic) the token estimate skews high — treat it as an
  order-of-magnitude signal, not an invoice.
- Only retrieval that serves *content* records usage (`ask`, `search`, `get_summary`,
  `read_file`); UI navigation like the sidebar path filter deliberately does not.

## What the engine bar's memory numbers mean

The engine bar reports memory the way the resource engine reasons about it, not the way Activity
Monitor does — these are deliberately different numbers:

- **"free"** is the **model budget**: room for a new model to load *above* the keep-free headroom
  band (`total − used − headroom`, clamped at 0). It is not OS "free RAM". On macOS the OS keeps
  memory resident as reclaimable cache ("free RAM is wasted RAM"), so OS-free reads as near-zero
  even when there is plenty of room for another model.
- **"used"** excludes that reclaimable cache, for the same reason.
- **Pressure** (`memory ok` / `memory tight` / `memory low`) is derived from the budget, **not from
  swap**. Earlier builds labeled it from swap percentage, which was misleading on a healthy machine
  that pages lazily; the swap figure is no longer surfaced.
- **Release models** (`POST /api/engine/release`) unloads only **Indexa's own** loaded Ollama
  models (`keep_alive=0` eviction). It is **not a system purge** — it cannot and does not touch
  other processes' memory, and the freed RAM appears only as Ollama actually evicts.

## What `retrieval coverage` on an answer means

`ask` labels each answer **high / medium / low** from the *retrieval evidence*, before synthesis:
how many hits came back relative to the request, how strong the top fusion score is, whether
keyword and semantic retrieval corroborate each other, and how steep the drop-off is. The basis
is stated next to the label ("4 moderate matches").

It is deliberately called **retrieval coverage**, not "confidence" — it is a **heuristic read of
the retrieval evidence, not a calibrated probability**. "high" means the retrieved sources cover
the question well, not that the synthesized answer is 90% likely to be correct. The LLM can still
mis-synthesize from good evidence (and the sources are always listed so you can check it).
`ask --explain` prints the inputs behind the label. (The `--json` / MCP / SSE field stays named
`confidence` for backward compatibility; only the human-facing label changed.)

## What's opt-in (not default)

| Feature | Why opt-in | How to enable |
|---|---|---|
| **Whisper transcription** (audio) | Requires a ~150MB model + compute | `[parsers.audio] transcribe = true` |
| **Vision captioning** (images) | Requires a vision model | `[parsers.image] caption = true` |
| **OCR** (scanned PDFs) | Requires poppler (`pdftoppm`) + Tesseract CLI | `[parsers.pdf] backend = "ocr"` |
| **Cross-encoder re-ranking** | Downloads a ~85 MB DeBERTa-v2 model on first use | `[retrieval] rerank_backend = "cross-encoder"` (base `rerank` is on by default) |
| **Contextual retrieval** | LLM call per chunk at index time (expensive) | `[describer] contextual_retrieval = true` |
| **Cloud embeddings** | Requires API key, costs money | `[embedding] provider = "openai"` |
| **Cloud LLM** | Requires API key | `[describer] provider = "anthropic"` |

All four media paths — OCR (poppler + Tesseract), audio transcription (whisper-cli), video-frame
extraction (ffmpeg), and image captioning (an Ollama vision model) — have happy-path end-to-end
tests gated behind `--ignored` (`crates/parsers/tests/multimodal_live.rs`,
`crates/llm/tests/caption_live.rs`); run them with the external tools installed to verify the
integrations on real media. **Vision-model note:** the default caption model `gemma3:4b` (a Gemma-3
vision-language model) loads on a stock Ollama; some Ollama builds reject `llama3.2-vision`
("unknown model architecture: `mllama`") — set `[parsers.image] model = "moondream"` (or keep the
default) if you hit that.

---

## Near-duplicate detection accuracy

`insights duplicates` clusters by summary-embedding cosine similarity. Up to ~2,000 summarized
files the comparison is exhaustive (exact). Past that, candidate pairs come from locality-sensitive
hashing (random-hyperplane signatures, deterministic seed), then exact cosine verification — which
means **pairs whose similarity sits near the threshold can be missed** (recall ≈93% at the 0.95
default, approaching 100% as similarity → 1). False positives are not possible (every candidate is
exactly verified), and exact-duplicate grouping (identical content hashes) is always exhaustive.

## Freshness limits of incremental re-summarize

Refresh skips files whose full-content hash matches the stored summary's — cheap and exact. But the
*pre-filter* that decides which files get hashed is mtime-based (`modified_s >= generated_at`), so a
change that **preserves the file's mtime** (rsync `-t`, `tar -x`, `cp -p`, some sync clients) is not
re-examined until something else touches it. The web "Regenerate" action bypasses all of this
(clears stored hashes) when you need certainty.

## Decision log

Changes to defaults are recorded here with rationale.

| Date | Decision | Rationale |
|---|---|---|
| 2025-05 | Default embedding: `nomic-embed-text` via Ollama | Apache-2.0, strong MTEB, zero ops, local-first |
| 2026-06 | Near-dup candidates via LSH above 2,000 files | O(n²) cosine silently capped at 5K files — the cap was worse than disclosed approximation; exact verify keeps precision at 100% |
| 2025-05 | PDF parser: `pdf-extract` (pure Rust) | No C++ build dep, sufficient for text-layer PDFs |
| 2025-05 | Vector storage: SQLite f32 blobs | Zero ops, single file, adequate for <300K vectors |
| 2025-05 | Fusion: RRF k=60 | Parameter-free, matches industry defaults, robust |
| 2025-05 | No re-ranker by default | Latency cost > recall benefit at top_k=8 |
| 2026-06-16 | Re-ranking now **on** by default (`rerank_backend = "llm"`); `top_k` 8 → 12 | The listwise LLM re-ranker reuses the already-loaded generation model (no second model, no download) and fails open, so the latency cost is one extra call for a measurable recall gain; the v0.43 cross-encoder backend stays opt-in (~85 MB download) |
| 2026-05-28 | Default describer: `gemma2:9b` (was `qwen2.5:14b`) | Google, Apache-2.0; user preference for non-Chinese-company defaults |
| 2026-05-28 | Default describer upgraded: `gemma3:12b` / `gemma3:4b` (was `gemma2:9b` / `gemma2:2b`) | Gemma 3 (March 2025) outperforms Gemma 2 at same parameter count; available on Ollama |
| 2026-05-28 | Added `google` embedding provider | Google `text-embedding-004` matches nomic-embed-text dim (768), state-of-the-art quality |
| 2026-06-11 | Call-graph queries resolve calls by tier (same-file → same-dir → import → bare) at query time; strict = drop bare tier (was: unique-definition name filter) | Bare-name matching's worst false positive — a local helper named like a popular symbol fanning out repo-wide — is structural, not statistical; tiered resolution removes it without re-indexing, never invents edges, and confines the caveat to the labeled bare remainder |
