# Configuration Reference

Indexa reads its configuration from a TOML file. The default path is:

| Platform | Default path |
|---|---|
| macOS | `~/Library/Application Support/indexa/indexa/config.toml` |
| Linux | `~/.config/indexa/config.toml` (XDG) |
| Windows | `%APPDATA%\indexa\indexa\config.toml` |

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

## Retrieval

Controls how search results are ranked and how many are returned.

```toml
[retrieval]
hybrid = "rrf"   # rrf | sparse | dense | weighted
rrf_k  = 60      # RRF rank constant (higher = less weight to top ranks)
top_k  = 8       # results to retrieve before reranking
rerank = false   # enable cross-encoder reranking (adds ~200ms)
```

### Hybrid modes

| Mode | Description |
|---|---|
| `rrf` | **Default.** Reciprocal Rank Fusion — combines sparse (BM25) and dense (cosine) results parameter-free. |
| `sparse` | Full-text search only (BM25/FTS5). |
| `dense` | Semantic search only (cosine similarity). |
| `weighted` | Future: weighted linear combination. |

---

## Describer (LLM for answer synthesis)

Controls the LLM used to generate answers in `indexa ask` and the web UI.

```toml
[describer]
provider              = "ollama"
model                 = "gemma2:9b"   # default: Google gemma2:9b (Apache-2.0)
base_url              = "http://localhost:11434"
contextual_retrieval  = false   # Anthropic-style per-chunk prefix at index time
```

### Providers

| Provider | Notes |
|---|---|
| `ollama` | Default. Any chat model in Ollama. URL override: `OLLAMA_HOST` env var. |
| `openai` | Requires `OPENAI_API_KEY`. URL override: `OPENAI_BASE_URL`. Recommended: `gpt-4o-mini`. |
| `anthropic` | Requires `ANTHROPIC_API_KEY`. Recommended: `claude-haiku-4-5-20251001`. |
| `llamacpp` | llama.cpp in OpenAI-compat mode. Set `base_url` or `OPENAI_BASE_URL`. |

---

## Parser overrides

Fine-tune how specific file types are handled.

```toml
[parsers.pdf]
backend = "pdfium"  # pdfium (default, pure Rust) | marker (scanned PDFs, requires Marker CLI)

[parsers.image]
caption = false  # set true to enable vision-model captioning (future)

[parsers.audio]
transcribe = false  # set true to enable whisper.cpp transcription (future)
```

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

[describer]
provider = "ollama"
model    = "gemma2:9b"

[[region]]
path = "~/Documents/Voice Memos"
[region.parsers.audio]
transcribe = true

[[region]]
path = "~/Pictures"
[region.parsers.image]
caption = true
```
