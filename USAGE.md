# Indexa — Usage Guide

The complete how-to for Indexa, **the local context engine for AI** — the tool that reads your code or
your disk once, on your machine, and hands any AI tool the slice it needs instead of the whole repo
(saving cloud tokens and giving local models context they can't hold). If you just want the 30-second
version, see the [README](README.md). This guide covers every command, the full config, the MCP setup,
and common recipes.

---

## 1. Install

### Pre-built binary (recommended)

```bash
# macOS (Apple Silicon)
curl -L -o /usr/local/bin/indexa \
  https://github.com/harf-promo/indexa/releases/latest/download/indexa-aarch64-apple-darwin
chmod +x /usr/local/bin/indexa
xattr -d com.apple.quarantine /usr/local/bin/indexa   # if Gatekeeper prompts

# macOS (Intel)  → indexa-x86_64-apple-darwin
# Linux x86_64   → indexa-x86_64-linux-gnu
# Linux arm64    → indexa-aarch64-linux-gnu
# Windows x86_64 → indexa-x86_64-windows.exe
```

### From source (Rust ≥ 1.82)

```bash
git clone https://github.com/harf-promo/indexa
cd indexa
cargo install --path apps/indexa      # installs `indexa` onto your PATH (~/.cargo/bin)
# or: cargo build --release           # binary at target/release/indexa
```

### Prerequisites — local models (Ollama)

Indexa defaults to local models via [Ollama](https://ollama.com). Pull these once:

```bash
ollama pull nomic-embed-text   # embeddings (~270 MB)
ollama pull gemma3:4b          # per-file summaries (~3 GB)
ollama pull gemma3:12b         # directory roll-ups + Q&A (~8 GB)
ollama list                    # verify they're installed
```

You can use cloud models instead (OpenAI / Anthropic / Google) — see [§6 Adapters](#6-ai-adapters).
Run `indexa doctor` to see which models fit your machine's memory.

---

## 2. The mental model

Indexa builds context in layers. **The index is the substrate; context is the product.**

```
   scan  ─────────────►  deep  ─────────────►  summarize  ─────────────►  use it
   (surface map,          (parse + embed         (per-file + rolled-up      ask · export ·
    zero AI)               file content)          hierarchical summaries)    serve · mcp
```

- **Surface scan** — walks the tree, classifies regions (code / media / system / build-artifact),
  zero AI calls. Instant context map.
- **Deep context** — parses file content (code via tree-sitter, PDF text, EXIF, etc.), chunks it,
  computes embeddings. This is what makes search work.
- **Summaries** — an LLM writes a 1–2 sentence summary per file, then rolls them up bottom-up so every
  folder (and the whole disk) has a hierarchical context graph.
- **Use it** — `ask` questions, `export` a context file for any AI tool, `serve` a web UI, or expose it
  to AI agents over `mcp`.

Everything lives in one SQLite file named `index.db`:

| Platform | Index database |
|---|---|
| macOS | `~/Library/Application Support/dev.indexa.Indexa/index.db` |
| Linux | `~/.local/share/indexa/index.db` |
| Windows | `%LOCALAPPDATA%\indexa\Indexa\data\index.db` |

---

## 3. Core flows

### One repo, fed to your AI coding tool

```bash
indexa scan ~/code/my-monorepo
indexa deep ~/code/my-monorepo
indexa summarize ~/code/my-monorepo
indexa export ~/code/my-monorepo --format xml > .context.xml
claude "given @.context.xml, find the auth flow and add MFA"
```

The paid model spends its budget on the change you want — not on re-reading your folder tree.

### Your whole disk, as a personal AI memory

```bash
indexa scan --all                 # surface-map everything (fast, no AI)
indexa deep ~/Documents ~/code    # deep-index the regions worth understanding
indexa summarize ~/Documents
indexa ask "where are my tax documents from last year?"
indexa ask "which of my projects use Postgres?"
```

On a memory-tight machine, set a gentler profile first: `[resource] profile = "conservative"` (see §5),
or let `auto_select_model` pick a lighter model.

### The web workspace

```bash
indexa serve                      # opens http://localhost:7620
```

One **Context workspace** with a Context / Map / Ask toggle and a docked Ask bar; an always-on **Engine
bar** showing live CPU / RAM / memory-pressure (and live build progress during a job); Settings behind
the topbar gear; jobs in the ▸ Activity drawer.

### Keep context fresh

```bash
indexa watch ~/code/my-monorepo   # re-parses + re-embeds changed files via filesystem events
indexa worker                     # drains the summarization queue in the background
```

---

## 4. Full CLI reference

Global flag (all commands): `--config <PATH>` overrides the default config location.

| Command | Key flags | What it does |
|---|---|---|
| `scan [paths…]` | `--all` | Surface walk → classify regions. `--all` scans the home dir / whole machine. |
| `map` | `--depth N` (3) | Print a summary map of regions by category. |
| `deep [paths…]` | `--embed-model`, `--dry-run`, `--mode <augment\|compress\|summaries-only>` | Parse + chunk + embed file content; enqueues summarization. |
| `summarize [paths…]` | `--mode`, `--passes N` | Generate file + directory summaries synchronously. |
| `describe <path>` | — | Print a node's summary + ancestor breadcrumb chain + children. |
| `worker` | `--concurrency N` (2) | Background daemon draining the summary queue. |
| `export [paths…]` | `--format <xml\|md\|json>`, `--depth N`, `--output FILE`, `--changed-since <7d\|12h\|90m>`, `--category <cat>` | Render the summary tree as AI-ready context (XML is primary). Slice it with `--changed-since` (recency) and/or `--category` to export just the relevant part. |
| `ask <question>` | `--scope PATH`, `--top-k N`, `--sparse-only`, `--dense-only`, `--embed-model`, `--llm-model`, `--no-synthesize` | Hybrid retrieval + LLM-synthesized answer with sources. `--no-synthesize` skips local synthesis and prints the retrieved context slice for you to answer with your own model (also `ask {synthesize:false}` over MCP). |
| `watch [paths…]` | `--embed-model` | Keep context current via filesystem events. |
| `serve` | `--port N` (7620), `--embed-model`, `--llm-model` | Local web UI. |
| `mcp` | — | Run the MCP stdio server for AI agents (see §7). `mcp install [--client …]` registers Indexa with your AI clients (auto-detects when no `--client`). |
| `completion <shell>` | — | Print a shell-completion script (`bash`/`zsh`/`fish`/…). |
| `status` | `--unknown`, `--deep`, `--json` | Index stats (+ token-savings by tool); `--deep` adds a coverage health report; `--unknown` lists top unclassified extensions. |
| `rm <paths…>` | `-r/--recursive` | Remove paths from the index (files on disk are untouched). |
| `doctor` | `--profile`, `--files N`, `--chunks N`, `--latency` | Machine specs, per-model memory fit, ETA estimates, Ollama env checks. `--latency` times a tiny embed + generate to catch a slow Ollama. |
| `fingerprint` | `--paths` | Detect installed software / project types by file-pattern signatures. |
| `classify` | `--paths`, `--category <cat>` | Suggest a semantic category (work/personal/archive/media/code/system) per folder. |
| `report [questions…]` | `--saved NAME`, `--format <md\|xml>`, `--output FILE` | Run several `ask` questions and render one cited document (TOC + answers + sources). |
| `insights <largest\|languages\|duplicates\|stale\|diff>` | `--json`, `--threshold`, `--days` | Analytical reports computed directly from the index — no AI calls. |

> **Why both `report` and `insights`?** They answer different questions with different engines.
> `report` is a multi-question **ask** digest — retrieval + LLM synthesis rendered as one citable document (an onboarding / design-doc generator).
> `insights` is **index analytics** — pure SQLite queries over what's indexed (duplicates, stale dirs, sizes, languages, weekly diff); no LLM, no retrieval.

---

## 5. Configuration (`config.toml`)

Location: `~/Library/Application Support/dev.indexa.Indexa/config.toml` (macOS) /
`~/.config/indexa/config.toml` (Linux XDG). Every field has a default; unknown keys are ignored. Run
`indexa status` to print the exact path.

```toml
[embedding]
provider = "ollama"          # ollama | openai | llamacpp | google
model = "nomic-embed-text"
dim = 768
base_url = "http://localhost:11434"

[chunking]
strategy = "structure"       # structure (code/markdown-aware) | fixed
size = 800                   # target words per chunk
overlap = 100                # word overlap between fixed chunks

[retrieval]
hybrid = "rrf"               # rrf (fuse sparse+dense) | sparse | dense
rrf_k = 60                   # RRF constant
top_k = 12                   # passages retrieved before reranking
rerank = true                # rerank before synthesis (default on; reuses the gen model, fails open)
rerank_backend = "llm"       # "llm" (listwise, no download) | "cross-encoder" (~85 MB DeBERTa-v2)
summary_weight = 0.0         # >0 boosts chunks under summary-matched dirs
summary_depth_alpha = 0.15   # shallower summaries get a broader-context boost
context_budget = 8000        # max chars packed into the answer prompt

[describer]
provider = "ollama"          # ollama | openai | anthropic | llamacpp
model = "gemma3:12b"         # Q&A answer model
file_model = "gemma3:4b"     # per-file summaries
dir_model = "gemma3:12b"     # directory roll-ups
num_ctx = 4096               # Ollama KV-cache window — keep aligned with the memory budget
base_url = "http://localhost:11434"
contextual_retrieval = false # Anthropic-style per-chunk context prefix at index time
mode = "augment"             # augment | compress | summaries-only
queue_concurrency = 2
max_children_per_summary = 30
passes_first = 2             # refinement passes for a brand-new summary
passes_refresh = 1           # passes when refreshing
passes_cap = 3               # hard ceiling

[parsers]
max_file_mb = 100            # 0 = no cap
[parsers.pdf]
backend = "pdfium"           # pdfium | marker (scanned/OCR)
[parsers.image]
caption = false              # opt-in (roadmap: local vision captions)
[parsers.audio]
transcribe = false           # opt-in (roadmap: local transcription)

[resource]
profile = "balanced"         # conservative | balanced | performance
headroom_gb = 0.0            # 0 = use profile default
auto_select_model = true     # downgrade the model if the preferred one won't fit RAM
keep_alive_secs = 0          # 0 = profile default
micro_benchmark = true       # measure real throughput at job start for accurate ETAs

[api_keys]                   # fallback when the matching env var is unset; stored 0600
openai = ""
anthropic = ""
google = ""
```

**Storage modes** (`[describer] mode`): `augment` (chunks + summaries, best recall) · `compress`
(summarize, then drop chunks — ~10× smaller) · `summaries-only` (skip chunking — ~100× smaller, no
hybrid retrieval; ~3.5 GB per 1 TB indexed).

**Resource profiles:** `conservative` (8 GB headroom, gentlest) · `balanced` (5 GB, default) ·
`performance` (3 GB, fastest/heaviest). The memory watchdog pauses LLM/embed work under genuine pressure
and resumes automatically — it won't freeze your machine.

### Environment variables

| Variable | Effect |
|---|---|
| `OLLAMA_HOST` | Override the Ollama server URL for all local model calls. |
| `OPENAI_API_KEY` / `OPENAI_BASE_URL` | OpenAI key / endpoint (also covers OpenAI-compatible servers). |
| `ANTHROPIC_API_KEY` | Anthropic key. |
| `GOOGLE_API_KEY` / `GOOGLE_BASE_URL` | Google Gemini embedding key / endpoint. |
| `INDEXA_WEB_ALLOW_KEY_EDIT=1` | Required to edit API keys from the web Settings UI. |

---

## 6. AI adapters

Bring your own model — none is bundled.

| Adapter | Runs | Notes |
|---|---|---|
| **Ollama** | Local, offline | Default. `OLLAMA_HOST` to point elsewhere. |
| **llama.cpp** | Local | Via its OpenAI-compatible HTTP server. |
| **Claude subscription** | Cloud (your plan) | `provider = "claude-code"` — runs on your Claude Pro/Max plan via the local `claude` CLI, no API key. |
| **Google Gemini** | Cloud | Embeddings (`text-embedding-004`) match local quality. |
| **OpenAI** | Cloud | Data leaves your device. |
| **Anthropic** | Cloud | Data leaves your device (answers/summaries). |

**Optional reranking** — set `[retrieval] rerank = true` to add a cross-encoder reorder pass before the
answer. Off by default and *fails open*: any model hiccup falls back to the original order.

### Use your Claude Pro/Max subscription (no API key)

If you subscribe to Claude Pro or Max, Indexa can run summaries and answers on your **subscription** via
the local `claude` CLI — no API key, no per-token billing:

```toml
# config.toml
[describer]
provider = "claude-code"
model = "sonnet"        # answers (the `ask` path)
file_model = "sonnet"   # per-file summaries
dir_model = "sonnet"    # directory roll-ups
```

**Auth.** Just be logged into Claude Code on the machine (`claude login`). Indexa shells out to
`claude -p … --output-format json`, which reuses that session and draws from your plan, not the metered
API. For a headless server, mint a token with `claude setup-token` (sets `CLAUDE_CODE_OAUTH_TOKEN`).

**Caveats.** Each call spawns a short-lived `claude` process (~1–3 s startup), so for whole-disk **bulk**
summarization local Ollama is faster; `claude-code` shines for `ask` and targeted summaries. **Embeddings
always stay local** (Ollama `nomic-embed-text`) — the `claude` CLI has no embedding endpoint.

Check it anytime: `indexa doctor` prints a Claude-provider block (CLI present? signed in? which plan?), and
the web Settings panel shows the same under **Claude subscription**.

---

## 7. MCP — let AI agents query your context live

Instead of exporting a file, run the MCP server and your agent queries the live index on demand.

```bash
indexa mcp        # stdio Model Context Protocol server
```

Add it to **Claude Desktop** (`claude_desktop_config.json`) — or any MCP client (Cursor, etc.):

```json
{
  "mcpServers": {
    "indexa": { "command": "indexa", "args": ["mcp"] }
  }
}
```

**46 tools** are exposed (see [docs/how-to/live-retrieval-over-mcp.md](docs/how-to/live-retrieval-over-mcp.md)
for the full table). The ones you'll reach for most: `search` (content search), `browse_tree` (one
directory level), `get_summary` (`tier` = l0 one-liner / l1 full+children / l2 file content —
progressive disclosure), `get_chunk_context` (a file's indexed chunks, or the neighbors of a search
hit), `read_file` (content, confined to indexed roots), `ask` (full retrieval+answer pipeline),
`dependencies` / `who_imports` / `who_calls` / `blast_radius` (the code graph), `query_config`
(effective config, no secrets), and `get_stats`.

A local agent pulls `get_summary("auth")` on demand instead of pre-loading the repo — staying coherent
across long tasks without hitting the context-window cliff.

### Install Indexa into Claude Code

To let Claude Code (your Sonnet-backed agent) query your indexed context live, register Indexa's MCP
server once:

```bash
claude mcp add -s user indexa -- indexa mcp   # register the stdio server (user scope)
claude mcp list                               # verify: "indexa  - ✓ Connected"
claude mcp get indexa                         # show the command/args
```

Any Claude Code session can then call Indexa's `search` / `ask` / `browse_tree` / `get_summary` /
`read_file` / `get_stats` tools against your local index.

---

## 8. Recipes

**Feed a repo to Claude Code / Cursor / Codex.** Scan → deep → summarize → `export --format xml` →
reference the file in your prompt. (Or skip the file and wire up `indexa mcp`.)

**Ask across your whole disk.** `indexa scan --all`, deep-index the regions you care about, then
`indexa ask "…"`. Use `--scope ~/code` to confine an answer to one area.

**Keep a project's context fresh.** Leave `indexa watch ~/code/project` running, plus `indexa worker`
to drain summaries in the background. (Roadmap: a native desktop app replaces leaving terminals open.)

**Right-size for a small machine.** `indexa doctor` shows what fits; set `[resource] profile =
"conservative"` and/or `mode = "summaries-only"`; keep `auto_select_model = true`.

**Keep dense search fast on a huge index.** Past ~50K chunks, brute-force cosine starts to drag.
Set `[retrieval] ann = true` to switch dense retrieval to an in-memory HNSW index (it only engages
above `ann_min_chunks`, default 50 000 — below that brute-force is faster than building the index).

**Index in the background.** Leave `indexa worker` draining the summary queue, and `indexa watch <path>`
re-embedding files as you save them — no foreground build to babysit. `indexa worker --auto-reindex`
also re-scans roots that go stale past `[scan] auto_reindex`.

**Share one topic, not the whole repo.** Group scattered files into a named **Context Pack** and export
just that bundle: `indexa pack create "Auth" --auto` → `indexa pack export "Auth" --format xml > auth.xml`.
Paste `auth.xml` into any AI tool — it's the subject, not the directory tree.

**Debug a wrong answer.** `indexa ask --explain "…"` prints the sparse + dense + fused/reranked hits with
scores, so you can see exactly which sources were chosen and why (see TROUBLESHOOTING → *Ask returns
irrelevant results*).

**Feed a small local model.** Point Ollama/llama.cpp at Indexa instead of stuffing whole files into a
tiny context window: register `indexa mcp` (or export a slice) so the model retrieves a bounded, relevant
context on demand — the KV-cache stays small and the model stays coherent. Caveats live in
[docs/methodology.md](docs/methodology.md).

**Get shell completions.** `indexa completion zsh > "${fpath[1]}/_indexa"` (or `bash`/`fish`).

---

## 9. Extending Indexa

Three seams take new capabilities without forking the core. Each is small and has a worked example in
the tree; the contract is "match the existing pattern in the same file."

- **A new file parser.** Implement `indexa_parsers::Parser` (`accepts_path` / `accepts_mime` / `parse`)
  and register it with `Registry::register` — it's checked before the built-ins. See the worked plugin
  example at the top of `crates/parsers/src/registry.rs`, and mirror the **graceful-degradation
  contract**: malformed input returns a stub chunk, never `Err`/panic (`crates/parsers/tests/error_cases.rs`).
- **A new web handler.** Add the async fn under `crates/web/src/handlers/`, export it from `handlers/mod.rs`,
  and register the route in `build_router` (`crates/web/src/lib.rs`). Test it through the real router with
  the `build_router` + `tower::oneshot` harness already in `lib.rs` (no live server needed).
- **A new MCP tool.** Add a `#[tool(description = …)]` method to the relevant `#[tool_router]` impl in
  `crates/mcp/src/` (template: `admin.rs`), opening a fresh `Store` via `self.store()`. Then **regenerate
  the golden list** (`INDEXA_UPDATE_GOLDEN=1 cargo test -p indexa-mcp`) and bump the "N tools" count in
  `README.md` / `CLAUDE.md` / `docs/how-to/live-retrieval-over-mcp.md` — the `doc_tool_count_matches_code`
  test fails the build otherwise.

A new web `NN-*.js` / `NN-*.css` asset is **dead unless added to the `include_str!` concat list** in
`crates/web/src/lib.rs`.

---

## Troubleshooting

The quick hits are below; the full guide is [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md).

- **`indexa: command not found`** after `cargo build` — the binary is at `target/release/indexa`. Run
  `cargo install --path apps/indexa` to put it on your PATH.
- **Ollama not reachable** — start Ollama; if it's on another host/port set `OLLAMA_HOST`.
- **Mid-build "easing off for memory"** — expected on tight RAM; the watchdog is protecting you. Switch
  to a lighter model (the web "ask me first" popover offers this) or a gentler resource profile.
- **Answers say "run indexa deep"** — that path is scanned but not deep-indexed yet; run `indexa deep <path>`.

See also: [README](README.md) · [ROADMAP](ROADMAP.md) · [docs/methodology.md](docs/methodology.md)
(how retrieval keeps the slice relevant — honest trade-offs) · [docs/COMPETITIVE.md](docs/COMPETITIVE.md).
