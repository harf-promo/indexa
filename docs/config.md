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
```

> `respect_gitignore` reads the scan root's own `.gitignore`; nested per-subdirectory `.gitignore`
> files are not separately loaded. `ignore` patterns use gitignore syntax (globs, `dir/`, `!negation`).
> Anything skipped here is never walked, so it can't be indexed or summarized. Use
> [`indexa prune`](#) to clean rows left from content that *was* indexed before you ignored it.

---

## Retrieval

Controls how search results are ranked and how many are returned.

```toml
[retrieval]
hybrid               = "rrf"  # rrf | sparse | dense
rrf_k                = 60     # RRF rank constant (higher = less weight to top ranks)
top_k                = 8      # results to retrieve before reranking
rerank               = false  # enable cross-encoder reranking (one extra local-model call; fails open)
summary_weight       = 0.0    # 0.0 disables the parent-summary boost; >0 blends folder-summary similarity into ranking
summary_depth_alpha  = 0.15   # depth-boost coefficient for summary-aware retrieval
context_budget       = 4000   # max characters of retrieved context packed into the answer prompt
use_weights          = true   # apply per-file/dir/category importance weights as a multiplicative boost
ann                  = false  # use an in-memory HNSW index for dense retrieval (vs brute-force cosine)
ann_min_chunks       = 50000  # only build/use the ANN index above this chunk count
agentic              = false  # default `ask` to the agentic multi-hop loop (per-call: --agentic / MCP agentic)
agentic_max_steps    = 3      # max retrieval hops in agentic mode (clamped 1..=5)
```

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
caption = false  # set true to enable vision-model captioning (future)

[parsers.audio]
transcribe = false  # set true to enable whisper.cpp transcription (future)
```

> **PDF:** text extraction currently uses the pure-Rust [`pdf-extract`](https://crates.io/crates/pdf-extract)
> crate (no native dependency). OCR for scanned / image-only PDFs is planned but not yet wired, so
> image-only PDFs currently yield little or no text.

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
top_k  = 10
rerank = false

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
