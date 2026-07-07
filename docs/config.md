# Configuration Reference

Indexa reads its configuration from a TOML file. The default path is:

| Platform | Default path |
|---|---|
| macOS | `~/Library/Application Support/dev.indexa.Indexa/config.toml` |
| Linux | `~/.config/indexa/config.toml` (XDG) |
| Windows | `%APPDATA%\indexa\Indexa\config.toml` |

(If the platform config directory can't be resolved, Indexa falls back to `~/.indexa/config.toml`.)

You can override the path with the `--config` flag:

```
indexa --config ~/my-indexa.toml ask "..."
```

**All fields are optional.** A missing or empty config file uses the defaults shown below.

---

## Embedding

Controls how file content is converted to semantic vectors.

```toml
[embedding]
provider = "ollama"              # ollama | openai | google | llamacpp
model    = "nomic-embed-text"    # model name (provider-specific)
dim      = 768                   # must match the model's output dimension
base_url = "http://localhost:11434"  # provider API base URL (optional — env var also works)
```

### Providers

| Provider | Notes |
|---|---|
| `ollama` | Default. Local server, no API key. URL override: `OLLAMA_HOST` env var. |
| `openai` | Requires `OPENAI_API_KEY`. URL override: `OPENAI_BASE_URL` env var. |
| `google` | Google Gemini. Requires `GOOGLE_API_KEY`. URL override: `GOOGLE_BASE_URL` env var. |
| `llamacpp` | llama.cpp in OpenAI-compatible mode. Set `base_url` or `OPENAI_BASE_URL`. |

### Recommended embedding models

| Model | Provider | Dim | Notes |
|---|---|---|---|
| `nomic-embed-text` | Ollama | 768 | Default. Apache-2.0, strong MTEB scores, local |
| `text-embedding-004` | Google | 768 | State-of-the-art, requires `GOOGLE_API_KEY` |
| `text-embedding-3-small` | OpenAI | 1536 | Good quality, ~$0.02/1M tokens |
| `text-embedding-3-large` | OpenAI | 3072 | Best quality, ~$0.13/1M tokens |

---

## Chunking

Controls how files are split into searchable pieces.

```toml
[chunking]
strategy = "structure"  # structure | fixed | recursive | semantic
size     = 800          # target words per chunk
overlap  = 100          # words of overlap between consecutive chunks
```

### Strategies

| Strategy | Description |
|---|---|
| `structure` | **Default.** Respects document structure: headings in Markdown, AST nodes in code, pages in PDFs. Falls back to fixed windows for plain text. |
| `fixed` | Fixed-size windows with `overlap` word overlap. Simple and predictable. |
| `recursive` | Future: split on paragraph/sentence boundaries. |
| `semantic` | Future: embed full document and window embeddings (late chunking). |

---

## Scan

Controls what the directory walker skips. On top of the built-in skips for build artifacts
(`node_modules`, `target`, `.venv`, `__pycache__`, `dist`, `.next`, …) and caches/VCS internals,
you can honor `.gitignore` and add your own patterns.

```toml
[scan]
respect_gitignore = true   # honor the scan root's .gitignore (its patterns, anchored at the root)
ignore            = []     # extra gitignore-style patterns, e.g. ["build/", "*.log", "vendor/"]
auto_reindex      = "off"  # "off" | "7d" | "30d" | "12h" … staleness interval for `worker --auto-reindex`
skip_binary       = false  # NUL-sniff files during deep; skip binaries (executables/images/blobs) from parsing
# threads         = 8      # walker worker threads; omit = all cores (min 4). Lower on a shared host.
```

> `respect_gitignore` reads the scan root's own `.gitignore`; nested per-subdirectory `.gitignore`
> files are not separately loaded. `ignore` patterns use gitignore syntax (globs, `dir/`, `!negation`).
> Anything skipped here is never walked, so it can't be indexed or summarized. Use
> [`indexa prune`](#) to clean rows left from content that *was* indexed before you ignored it.

### Scheduled / auto re-index

`auto_reindex` sets a **staleness interval**, not a scheduler. When you run:

```bash
indexa worker --auto-reindex
```

the worker first re-runs `scan → deep → summarize` for any indexed **root** whose newest content is
older than this interval (incremental — `deep` skips unchanged files, `summarize` refreshes stale
summaries), then drains the summary queue as usual. Roots that were never deep-indexed are skipped.

- The `--auto-reindex` **flag must be present** — an expensive rebuild never starts implicitly from
  the config value alone. If the flag is set but `auto_reindex = "off"`, it falls back to a 7-day interval.
- **To run it on a schedule, use cron** (the worker itself does the staleness check on each launch).
  For example, a nightly refresh that exits when the queue is drained is best expressed as a direct
  re-index of the roots you care about:

  ```cron
  # 3 AM daily — refresh a specific project (incremental; cheap if nothing changed)
  0 3 * * *  indexa index ~/code/myproject >> ~/.indexa-cron.log 2>&1
  ```

  Use `indexa worker --auto-reindex` when you want one long-running process that both keeps roots
  fresh and continuously drains summaries; use a cron'd `indexa index <path>` when you want a
  scheduled one-shot.

---

## Retrieval

Controls how search results are ranked and how many are returned.

```toml
[retrieval]
hybrid               = "rrf"  # rrf | sparse | dense
rrf_k                = 60     # RRF rank constant (higher = less weight to top ranks)
top_k                = 12     # results to retrieve before reranking
rerank               = true   # rerank hits before synthesis (default on; reuses the generation model — no extra dep — and fails open)
rerank_backend       = "llm"  # "llm" (listwise, no download) | "cross-encoder" (DeBERTa-v2 ~85 MB, downloaded on first use)
mmr_lambda           = 0.5    # diversity vs relevance when re-ranking (1.0 = relevance only / MMR off; 0.0 = max diversity)
summary_weight       = 0.0    # 0.0 disables the parent-summary boost; >0 blends folder-summary similarity into ranking
summary_depth_alpha  = 0.15   # depth-boost coefficient for summary-aware retrieval
context_budget       = 8000   # max characters of retrieved context packed into the answer prompt
use_weights          = true   # apply per-file/dir/category importance weights as a multiplicative boost
ann                  = false  # use an in-memory HNSW index for dense retrieval (vs brute-force cosine)
ann_min_chunks       = 50000  # only build/use the ANN index above this chunk count
agentic              = false  # default `ask` to the agentic multi-hop loop (per-call: --agentic / MCP agentic)
agentic_max_steps    = 3      # max retrieval hops in agentic mode (clamped 1..=5)
recency_boost        = false  # boost recently-modified files (mtime-based; off so it never silently re-ranks)
recency_days         = 90     # recency window in days (files older than this stay neutral when recency_boost is on)
archive_segments     = ["archive", "archived", "historical", "deprecated", "old"]  # path segments treated as historical
archive_penalty      = 0.15   # multiplicative down-weight for hits under an archive segment (0.0 disables it)
broad_per_file_cap   = 0      # 0 = off. >0 caps chunks-per-file for BROAD, unscoped questions only
graphrag_clusters    = false  # GraphRAG "Approach C": group a broad answer's hits into THEME clusters
graphrag_max_clusters = 4     # max clusters (also caps the per-cluster summary calls)
graphrag_cluster_sim = 0.55   # cosine threshold to join a hit to a cluster (higher = more clusters)
graphrag_summarize   = false  # also add a one-line LLM theme per cluster (extra calls; fail-open)
```

> `broad_per_file_cap` (v0.69+) only acts on broad/thematic, **unscoped** questions — focused and
> `--scope`d asks are never affected. When set (e.g. `2`), it stops a single chunk-dense file from
> monopolising a broad answer's context by reordering so other files get a turn (it never drops a
> hit — overflow just lands later in the budget). Leave it `0` unless broad answers on your corpus
> are dominated by one large file; on a file-diverse corpus there's nothing to balance.
>
> `graphrag_clusters` (v0.70+) likewise only acts on broad, **unscoped** questions: it groups the
> retrieved hits into semantic clusters and presents them under `=== THEME … ===` headers so the
> model can structure a multi-faceted answer (`graphrag_summarize` adds a one-line theme per cluster).
> The off path is byte-identical to flat packing and it fails open. Like the per-file cap it's a no-op
> on a topically-cohesive corpus (the hits collapse into one cluster) — enable it when a broad query
> on your files genuinely spans distinct topics.

> The summary-boost (`summary_weight`) only takes effect for dense/RRF modes and is off (0.0) by default.

> **Agentic retrieval** (`agentic`) runs a bounded *plan → search → refine* loop instead of a single
> retrieval — better for compositional questions, at the cost of a few extra model calls. It's opt-in
> per call (`indexa ask --agentic`, MCP `agentic: true`, or the web chat's "Agentic" checkbox); set
> `agentic = true` here to make it the default. It **fails open** to one-shot retrieval if the model
> won't emit the loop's actions. See [methodology.md](methodology.md#agentic-retrieval-opt-in).

### Hybrid modes

| Mode | Description |
|---|---|
| `rrf` | **Default.** Reciprocal Rank Fusion — combines sparse (BM25) and dense (cosine) results parameter-free. |
| `sparse` | Full-text search only (BM25/FTS5). |
| `dense` | Semantic search only (cosine similarity). |

### When to tune retrieval

Start with the defaults — they're good for most repos. Reach for these only when answers are off,
and change **one knob at a time**. Use `indexa ask --explain "<question>"` to see the sparse/dense/fused
rankings and confirm a change did what you expected.

| Symptom | Knob | Try |
|---|---|---|
| Answers miss relevant files that clearly exist | `top_k` | raise 12 → 16–20 (more candidates reach synthesis; costs a little context budget) |
| Answer cites too much noise / drifts off-topic | `top_k`, `context_budget` | lower `top_k` to 5–6; trim `context_budget` so only the strongest hits are packed |
| Exact keyword/identifier matches rank too low | `hybrid` | try `sparse`, or lower `rrf_k` (e.g. 30) to weight top ranks more heavily |
| Conceptual/paraphrased questions miss | `hybrid` | ensure `rrf` or `dense`, and that the folder was deep-indexed (embeddings exist) |
| Want folder-level topical relevance to count | `summary_weight` | raise from 0.0 to ~0.2–0.4 (dense/RRF only; blends parent-summary similarity) |
| One important dir keeps getting buried | `use_weights` + `indexa weight set` | boost that file/dir/category instead of globally re-tuning |
| Compositional question (needs several facts) | — | use `--agentic` per call rather than changing defaults |
| Long answers truncate context | `context_budget` | raise from 8000 (more chars packed into the prompt; watch the model's context window) |

`rrf_k` is the RRF rank constant: **higher** = ranks contribute more evenly (flatter), **lower** =
the very top hits dominate. The industry default of 60 rarely needs changing.

---

## Describer (LLM for answer synthesis)

Controls the LLM used to generate answers in `indexa ask` and the web UI.

```toml
[describer]
provider                 = "ollama"
model                    = "gemma3:12b"   # Q&A answer synthesis (Google gemma3:12b, Apache-2.0)
file_model               = "gemma3:4b"    # per-file summaries (smaller/faster)
dir_model                = "gemma3:12b"   # directory roll-up summaries (stronger model)
base_url                 = "http://localhost:11434"
contextual_retrieval     = false          # Anthropic-style per-chunk prefix at index time
mode                     = "augment"      # augment | compress | summaries-only
queue_concurrency        = 2              # concurrent summary worker tasks
max_children_per_summary = 30             # max child summaries fed into one directory roll-up
passes_first             = 2              # refinement passes when no prior summary exists
passes_refresh           = 1              # refinement passes when refreshing an existing summary
passes_cap               = 3              # hard ceiling on the `--passes` flag (values above are clamped)
```

`passes_*` implement multi-pass Self-Refine summarization: a first-time build runs `passes_first`
passes, a refresh runs `passes_refresh`, and any explicit `--passes` is clamped to `passes_cap`
(gains saturate after pass 2–3).

### Providers

| Provider | Notes |
|---|---|
| `ollama` | Default. Any chat model in Ollama. URL override: `OLLAMA_HOST` env var. |
| `openai` | Requires `OPENAI_API_KEY`. URL override: `OPENAI_BASE_URL`. Recommended: `gpt-4o-mini`. |
| `anthropic` | Requires `ANTHROPIC_API_KEY`. Recommended: `claude-haiku-4-5-20251001`. |
| `llamacpp` | llama.cpp in OpenAI-compat mode. Set `base_url` or `OPENAI_BASE_URL`. |

---

## Resource awareness

Controls how aggressively Indexa uses system memory during AI jobs. Indexa reads machine RAM and
swap pressure before and during `deep`/`summarize` and pauses work when the machine is under
memory pressure (the core of the macOS whole-machine-freeze fix). Run `indexa doctor` to see the
detected specs, per-model memory table, and ETA estimates.

```toml
[resource]
profile         = "balanced"   # conservative | balanced | performance
headroom_gb     = 0.0          # 0.0 = use the profile's built-in headroom; >0 overrides it (GB to keep free)
keep_alive_secs = 0            # 0 = use the profile default; how long Ollama keeps a model resident
```

| Profile | Behaviour |
|---|---|
| `conservative` | Largest memory headroom, shortest keep-alive — best on low-RAM machines. |
| `balanced` | **Default.** Sensible headroom and keep-alive for typical laptops. |
| `performance` | Smallest headroom, longest keep-alive — fastest on high-RAM machines. |

---

## Parser overrides

Fine-tune how specific file types are handled.

```toml
[parsers.image]
caption = false       # set true to caption images with a local vision model (opt-in)
model   = "gemma3:4b" # vision model to caption with (default: reuses the gemma3 summary model — no extra download)

[parsers.audio]
transcribe = false    # set true to transcribe audio via a whisper.cpp-style CLI (opt-in)
binary     = "whisper-cli"  # transcription binary on PATH (external tool)
model      = ""       # optional whisper model path passed to the binary

[parsers.video]
caption = false       # set true to caption sampled video frames with a local vision model (opt-in)
model   = "gemma3:4b" # vision model (default: gemma3 summary model)

[parsers.pdf]
backend    = "text"   # "text" = pdf-extract text layer only | "ocr" = also OCR scanned/image-only pages
ocr_binary = "tesseract"  # OCR engine when backend = "ocr" (external tool)
ocr_lang   = "eng"    # optional tesseract language hint, e.g. "eng" or "eng+ara"
```

> Captioning, transcription, and PDF OCR are all **opt-in and fail open** — when a model or external
> tool is missing, the file falls back to its text/empty result rather than erroring. They reuse the
> local models you already pulled (`gemma3` for vision) or shell out to external CLIs you install
> (`whisper-cli`, `tesseract` + `pdftoppm`/poppler), so nothing is auto-downloaded.

> **PDF:** text extraction uses the pure-Rust [`pdf-extract`](https://crates.io/crates/pdf-extract)
> crate (no native dependency) by default. Scanned / image-only PDFs have no text layer, so they
> yield little or no text under `backend = "text"`; set `backend = "ocr"` (and install poppler +
> tesseract) to recognise them.

---

## Per-region overrides

Apply different settings to different parts of your disk.

```toml
[[region]]
path = "~/Documents/Voice Memos"

[region.parsers.audio]
transcribe = true   # transcribe only voice memo files, not all audio

[[region]]
path = "~/Pictures"

[region.parsers.image]
caption = true   # enable vision captions for photos

[[region]]
path = "~/Work"

[region.embedding]
model    = "text-embedding-3-large"
provider = "openai"
dim      = 3072
```

The longest matching path prefix wins. In the example above, a file at `~/Documents/Voice Memos/meeting.m4a` matches the `Voice Memos` region, not the `~/Documents` region (if one existed), because `Voice Memos` is a longer prefix.

---

## Full example

```toml
[embedding]
provider = "ollama"
model    = "nomic-embed-text"
dim      = 768

[chunking]
strategy = "structure"
size     = 800
overlap  = 100

[retrieval]
hybrid = "rrf"
rrf_k  = 60
top_k  = 12
rerank = true

[resource]
profile = "balanced"   # conservative | balanced | performance

[describer]
provider = "ollama"
model    = "gemma3:12b"

[[region]]
path = "~/Documents/Voice Memos"
[region.parsers.audio]
transcribe = true

[[region]]
path = "~/Pictures"
[region.parsers.image]
caption = true
```
