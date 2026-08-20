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

## API keys

Optional cloud-provider API keys, persisted directly in `config.toml` as a fallback for the
`embedding`/`describer` `provider = "openai" | "anthropic" | "google"` settings above.

```toml
[api_keys]
# openai    = "sk-..."   # fallback for OPENAI_API_KEY
# anthropic = "sk-..."   # fallback for ANTHROPIC_API_KEY
# google    = "..."      # fallback for GOOGLE_API_KEY
```

> Unset by default — nothing is stored unless you set it here. The environment variable always
> wins when both are set; a key in `[api_keys]` only applies when its env var is absent. Setting
> any key here makes Indexa tighten `config.toml` to `0600` (owner read/write only) on load and on
> every save, and keys are never written to logs.

---

## Chunking

Controls how files are split into searchable pieces.

```toml
[chunking]
strategy = "structure"  # reserved — see below; today every value chunks the same way
size     = 800          # target words per chunk
overlap  = 100          # words of overlap between consecutive chunks
```

### Strategies

`strategy` is currently a **forward-looking / reserved field: nothing branches on it yet.**
`structure` / `fixed` / `recursive` / `semantic` all run today's same structure-aware word-window
chunker (headings in Markdown, AST nodes in code, pages in PDFs, falling back to fixed windows for
plain text); `size`/`overlap` apply the same way under every value. Setting it to anything other
than `structure` is accepted but has no effect yet:

| Strategy | Planned behavior |
|---|---|
| `structure` | **Default**, and today's only real behavior (see above). |
| `fixed` | Reserved: fixed-size windows only, no structure-awareness. |
| `recursive` | Reserved: split on paragraph/sentence boundaries. |
| `semantic` | Reserved: embed full document and window embeddings (late chunking). |

`indexa doctor` surfaces this too: it warns whenever `strategy` is set to anything other than
`structure`, naming the value you configured and reminding you it currently has no effect
(silent at the default, so an untouched config never sees it).

---

## Scan

Controls what the directory walker skips. On top of the built-in skips for build artifacts
(`node_modules`, `target`, `.venv`, `__pycache__`, `dist`, `.next`, …) and caches/VCS internals,
you can honor `.gitignore` and add your own patterns.

```toml
[scan]
respect_gitignore = true   # honor the scan root's .gitignore (its patterns, anchored at the root)
ignore            = []     # extra gitignore-style patterns, e.g. ["build/", "*.log", "vendor/"]
auto_reindex      = "off"  # "off" | "7d" | "30d" | "12h" … | "git-poll" — see Scheduled / auto re-index below
include_sensitive = false  # descend into .ssh/.gnupg/.aws/Keychains/browser profiles/… (also --include-sensitive)
redact_at_index   = true   # redact obvious secrets (API keys, tokens, PEM blocks) from chunk text at index time
skip_binary       = false  # NUL-sniff files during deep; skip binaries (executables/images/blobs) from parsing
custom_ignore     = true   # honor .indexaignore files (see below); set false to disable entirely
# threads         = 8      # walker worker threads; omit = all cores (min 4). Lower on a shared host.
```

> `respect_gitignore` reads the scan root's own `.gitignore`; nested per-subdirectory `.gitignore`
> files are not separately loaded. `ignore` patterns use gitignore syntax (globs, `dir/`, `!negation`).
> Anything skipped here is never walked, so it can't be indexed or summarized. Use
> [`indexa prune`](#) to clean rows left from content that *was* indexed before you ignored it.

### `.indexaignore` — tune indexing without touching git behavior

Drop a `.indexaignore` file (gitignore syntax) anywhere in the tree to add the **highest-precedence**
ignore layer — above `.gitignore` and `.ignore`, nested per directory like both. Because it's a
separate file, you can tune what Indexa indexes without changing what git tracks:

```gitignore
# .indexaignore
fixtures/          # exclude a noisy, committed fixtures dir from the index
!docs/generated/    # re-include a gitignored-but-valuable generated-docs dir
```

`!`-prefixed lines re-include a path even if `.gitignore`/`.ignore` excludes it — but this can never
re-include a sensitive credential store (`.ssh`, `.gnupg`, Keychains, browser profiles, …); that
prune is a separate, unconditional check. Gated on `respect_gitignore`; disable entirely with
`[scan] custom_ignore = false`.

### Scheduled / auto re-index

For an interval value (`"off"` / `"7d"` / `"30d"` / `"12h"` / …), `auto_reindex` sets a **staleness
interval**, not a scheduler. When you run:

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

**`auto_reindex = "git-poll"` is the one exception — it IS a continuous scheduler.** Instead of a
one-shot staleness check at launch, `indexa worker --auto-reindex` spawns a persistent background
task that watches each indexed git root's HEAD + tracked-tree dirtiness at an adaptive interval (5s,
growing with index size, capped at 60s) and re-indexes on change for the lifetime of the worker
process. A non-git root under git-poll mode falls back to the same interval-based staleness check
described above. The `--auto-reindex` flag must still be passed either way — an expensive rebuild
never starts implicitly from the config value alone.

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
rerank_model         = "mixedbread-ai/mxbai-rerank-xsmall-v1"  # HF model for rerank_backend = "cross-encoder"; ignored otherwise
mmr_lambda           = 0.5    # diversity vs relevance when re-ranking (1.0 = relevance only / MMR off; 0.0 = max diversity)
summary_weight       = 0.0    # 0.0 disables the parent-summary boost; >0 blends folder-summary similarity into ranking
summary_depth_alpha  = 0.15   # depth-boost coefficient for summary-aware retrieval
context_budget       = 8000   # max characters of retrieved context packed into the answer prompt
use_weights          = true   # apply per-file/dir/category importance weights as a multiplicative boost
ann                  = true   # on: HNSW dense index in serve/MCP above ann_min_chunks (else brute-force)
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
staleness_flags      = true   # flag cited files whose on-disk mtime is newer than what's indexed
query_predicates     = false  # recognize path:/ext:/type: predicates in free-text search/ask queries
```

> **`rerank_model`** (v0.77+) picks the HuggingFace cross-encoder used when `rerank_backend =
> "cross-encoder"` — ignored under the default `"llm"` backend. All three options share the same
> DeBERTa-v2 architecture (drop-in, larger = higher quality/download): `mxbai-rerank-xsmall-v1`
> (default, ~85 MB), `mxbai-rerank-base-v1` (~370 MB), `mxbai-rerank-large-v1` (~870 MB). Downloaded
> from HuggingFace on first use and cached in `~/.cache/huggingface/hub/`; a load failure falls open
> to `"llm"`.
>
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

> **Staleness attestation** (`staleness_flags`, default on): a cited file whose on-disk mtime is
> newer than what's indexed is flagged — "(stale: modified since indexed)" in `ask`/MCP text output,
> "(stale)" plus a footer count in `search`, a `stale: bool` field per source in the web JSON API.
> Annotation-only (never changes retrieval scores or which chunks are cited); each check is one
> `fs::metadata` call per cited file, fail-open on any I/O error.

> **Query predicates** (`query_predicates`, default off): recognize `path:<prefix>`,
> `ext:<extension>`, and `type:<name>` tokens in a free-text `search`/`ask` query and strip them
> out as filters instead of searching for them literally — e.g. `ext:md path:crates/core auth flow`
> searches "auth flow" scoped to `.md` files under `crates/core`. `path:` maps onto the existing
> scope filter (both `search` and `ask`); `ext:`/`type:` are a post-hoc hit filter (`search`
> only — `ask` synthesizes before a hit-level filter could apply, so they're stripped from the
> question but not enforced there). `type:<name>` is a ripgrep-style named file-type set — a
> curated multi-extension convenience over `ext:` (`type:python` == `.py` or `.pyi`); current
> sets: `rust`, `python`, `js`, `ts`, `go`, `java`, `c`, `cpp`, `web`, `docs`, `config`, `shell`,
> `ruby`, `php`, `swift`, `kotlin`. Unlike `path`/`ext` (any value accepted), `type` has a closed
> vocabulary — an unrecognized set name (`type:bogus`) is never treated as a predicate, so it
> passes through as ordinary text exactly like any other unrecognized `field:value` token. Off
> by default because it changes query interpretation; an unrecognized `field:value`-shaped token
> (anything not `path`/`ext`/a known `type` name) always passes through as ordinary text, so
> turning this on is safe even for queries that happen to contain a colon.

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
contextual_retrieval     = false          # Anthropic-style per-chunk LLM prefix at index time (one extra call per chunk)
contextual_prefix        = false          # DETERMINISTIC local sibling of contextual_retrieval — no LLM call, no extra cost; if both are set, contextual_retrieval wins
num_ctx                  = 4096           # Ollama num_ctx sent on every summarization/Q&A call — keep in sync with the resource budget's KV-cache assumption
mode                     = "augment"      # augment | compress | summaries-only
queue_concurrency        = 2              # concurrent summary worker tasks
max_children_per_summary = 30             # max child summaries fed into one directory roll-up
passes_first             = 2              # refinement passes when no prior summary exists
passes_refresh           = 1              # refinement passes when refreshing an existing summary
passes_cap               = 3              # hard ceiling on the `--passes` flag (values above are clamped)
claude_bin               = "claude"       # `claude` CLI path when provider = "claude-code"; empty = resolved on PATH
```

`passes_*` implement multi-pass Self-Refine summarization: a first-time build runs `passes_first`
passes, a refresh runs `passes_refresh`, and any explicit `--passes` is clamped to `passes_cap`
(gains saturate after pass 2–3).

`num_ctx` defaults to 4096 so Ollama's per-call KV-cache stays inside what the resource budget
assumes — omitting it (or raising it) lets Ollama fall back to its own default (32,768 for many
models), which can balloon the KV-cache roughly 8× past the budgeted footprint and drive swap
blowout on memory-constrained machines. Raise it only with matching headroom in `[resource]`.

### Providers

| Provider | Notes |
|---|---|
| `ollama` | Default. Any chat model in Ollama. URL override: `OLLAMA_HOST` env var. |
| `openai` | Requires `OPENAI_API_KEY`. URL override: `OPENAI_BASE_URL`. Recommended: `gpt-4o-mini`. |
| `anthropic` | Requires `ANTHROPIC_API_KEY`. Recommended: `claude-haiku-4-5-20251001`. |
| `llamacpp` | llama.cpp in OpenAI-compat mode. Set `base_url` or `OPENAI_BASE_URL`. |
| `claude-code` | Runs on your **Claude Pro/Max subscription** via the local `claude` CLI (`claude_bin`) — no API key, no per-token billing. See [Use your Claude Pro/Max subscription](../USAGE.md#use-your-claude-promax-subscription-no-api-key). |

---

## Resource awareness

Controls how aggressively Indexa uses system memory during AI jobs. Indexa reads machine RAM and
swap pressure before and during `deep`/`summarize` and pauses work when the machine is under
memory pressure (the core of the macOS whole-machine-freeze fix). Run `indexa doctor` to see the
detected specs, per-model memory table, and ETA estimates.

```toml
[resource]
profile           = "balanced"   # conservative | balanced | performance
headroom_gb       = 0.0          # 0.0 = use the profile's built-in headroom; >0 overrides it (GB to keep free)
auto_select_model = true         # downgrade to a smaller model if the preferred one won't fit the memory budget
keep_alive_secs   = 0            # 0 = use the profile default; how long Ollama keeps a model resident
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
[parsers]
max_file_mb = 100     # skip content parsing for files larger than this (MB); the entry is still
                       # recorded (metadata-only), just not opened/parsed. 0 disables the cap.
encoding = "auto"     # "auto" (default) transcodes UTF-16 (BOM-detected) to UTF-8 and lossy-
                       # decodes anything else; "utf-8" restores the old strict behavior (errors
                       # on invalid UTF-8) — see below.

[parsers.image]
caption = false       # set true to caption images with a local vision model (opt-in)
model   = "gemma3:4b" # vision model to caption with (default: reuses the gemma3 summary model — no extra download)

[parsers.audio]
transcribe = false    # set true to transcribe audio via a whisper.cpp-style CLI (opt-in)
binary     = "whisper-cli"  # transcription binary on PATH (external tool)
model      = ""       # optional whisper model path passed to the binary

[parsers.video]
caption     = false       # set true to caption sampled video frames with a local vision model (opt-in)
model       = "gemma3:4b" # vision model (default: gemma3 summary model)
binary      = "ffmpeg"    # ffmpeg binary on PATH, used for frame extraction (external tool)
fps_sample  = 0.5         # frames per second to sample (default: one frame every 2s)
max_frames  = 8           # max frames captioned per video (caps LLM cost)

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

> **Encoding (`[parsers] encoding`):** the plain-text and Markdown parsers previously required
> strict UTF-8 — a UTF-16 file (a common Windows artifact: PowerShell redirects, Notepad "Save as
> UTF-16", `.resx`, some logs/CSVs) failed to parse and was silently skipped. `encoding = "auto"`
> (the default) BOM-sniffs the file and transcodes UTF-16 to UTF-8 (lossy — invalid sequences
> become `U+FFFD`), falling back to lossy UTF-8 decoding for anything without a recognized BOM.
> A valid-UTF-8 file decodes identically either way, so `auto` only changes outcomes for files
> that previously errored. Set `encoding = "utf-8"` to restore the old strict behavior if you'd
> rather a malformed file be skipped than silently patched.

### Runtime preprocessor hooks (`[[parsers.preprocessor]]`)

```toml
[[parsers.preprocessor]]
glob           = "*.dwg"     # matched against the full path
command        = "dwg2text"  # run with the file's path as its argument + the file's bytes on stdin
timeout_s      = 30          # kill the command if it hasn't finished by then
max_output_mb  = 16          # cap on how much of its stdout is read
```

> **⚠ Security: `command` runs arbitrary code on every matching file.** There is deliberately no
> MCP tool or web endpoint to configure this — it's config-file only (the config file itself is
> 0600), so adding a hook is something only someone with local file access to your machine can do.
> Only point `command` at a tool you trust with the contents of every file it matches.
>
> Covers long-tail formats without shipping a parser — the external tool's stdout is indexed as
> plain text. Runs once per file **version** (the deep phase's mtime/hash skip applies), not once
> per query. Graceful fallback (mirrors ripgrep's `--pre`): a nonzero exit, empty stdout, or a
> timeout all fall through to whatever built-in parser would otherwise have handled the file — a
> broken or misconfigured hook never blanks a file a native parser could still extract from.
> `indexa doctor` lists any preprocessor hooks in effect.

### Transparent compressed content indexing (`[parsers] compressed`)

```toml
[parsers]
compressed = false   # set true to index the DECOMPRESSED content of standalone compressed files
```

> **Off by default.** When enabled, a standalone compressed file (`README.md.gz`, a rotated
> `access.log.gz`, `notes.md.zst`, `notes.md.xz`, `notes.md.br`, …) is decompressed and its
> content routed through the normal parser dispatch by its inner extension (`notes.md.gz` →
> `.md` → the Markdown parser), instead of being indexed as an opaque binary blob. Four codecs
> are supported: **gzip** (`.gz`), **zstd** (`.zst`), **xz/lzma** (`.xz`/`.lzma`), and **brotli**
> (`.br`) — all pure-Rust implementations, no C toolchain or system library required.
> `.tar.<codec>` and its short aliases (`.tgz`, `.tzst`, `.txz`) are unaffected either way —
> those are always handled by the archive parser, for every codec. Decompression is capped at
> the same size limit as zip archive entries — an oversized compressed file falls back to
> metadata-only rather than exhausting memory, and a file that turns out not to actually match
> its extension's codec also falls back cleanly.

---

## MCP server

```toml
[mcp]
tool_profile = "full"   # "full" (every tool advertised + callable) | "core" (a small task-focused subset)
```

> `core` narrows the advertised **and** callable tool surface to `search` / `ask` / `dependencies` /
> `who_calls` / `blast_radius` / `list_packs` / `search_pack` / `export_pack` / `add_note` /
> `list_open_decisions` — cuts the per-session tool-schema token cost for subagents doing bounded
> work; every other tool is un-advertised and rejected outright if called directly, not just hidden.
> Unrecognized values fall open to `full`. `indexa mcp --tool-profile core` overrides this
> per-invocation.

---

## Decision Ledger

Knobs for the questions `indexa classify`/scan opens when a judgment is too uncertain to apply
silently.

```toml
[review]
auto_record_below = 0.8    # auto judgments below this confidence become open questions instead of applying silently
max_open          = 50     # detectors stop opening new questions once this many are already open
max_new_per_scan  = 20     # max questions a single scan/classify pass may open
symbol_ambiguity  = false  # surface "which definition is authoritative?" for bare-name symbols defined in multiple files
```

> `symbol_ambiguity` is off by default — on idiomatic codebases (Rust `new`, `default`, `parse`,
> `build`, …) these questions are near-unanswerable and flood the inbox. Opt in only with a real
> polyglot symbol-resolution need.

---

## Remote sources

Opt-in ingestion for `indexa pack add-url` (pull a web page / GitHub issue or PR into a Context
Pack).

```toml
[sources]
enabled      = false  # allow pack add-url to fetch remote content (also: INDEXA_REMOTE_FETCH_ALLOW=1 per-run)
timeout_secs = 30     # HTTP timeout (seconds) for a remote fetch
max_retries  = 2      # retry attempts on transient HTTP failures (429/5xx/timeouts)
```

> **Off by default** — fetching a URL reaches the network, so it must be explicitly enabled here
> or via the environment variable.

---

## Model catalog

```toml
[models]
# catalog_url = "https://example.com/models.json"   # optional catalog JSON refreshed by POST /api/models/catalog/refresh
```

> Unset by default. When set, `POST /api/models/catalog/refresh` fetches this URL and replaces the
> served catalog; unset, that endpoint is a no-op and the bundled curated catalog is served. The
> fetch fails open — any error leaves the bundled/prior catalog in place.

---

## Code graph

```toml
[graph]
modules = false   # set true to expose the persisted architecture-map modules (4.6)
```

> **Off by default.** `modules` gates whether `code_graph`'s `modules: true` (MCP) and
> `indexa graph --modules` (CLI) report the persisted architecture map instead of an empty
> result — it does NOT control whether the map can be computed. Run `indexa graph
> --compute-modules <path>` any time (regardless of this flag) to cluster the code graph
> (Louvain, boosted by a same-directory prior) into named functional areas and label each with
> a short local-LLM call (`[describer] dir_model`, e.g. gemma3:12b) from its members' one-line
> summaries; a cluster too small or a label call that fails/rambles falls back to a
> deterministic name instead of blocking the computation. The web UI's Map → Graph view has a
> matching "Modules" toggle that reads the same persisted table.

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
```
