# Quickstart

In five minutes you'll point Indexa at a folder, watch it build a searchable context map on your
own machine, and ask it questions in plain language — or hand a context file to Claude Code, Cursor,
or any AI tool. Everything below runs locally; **nothing leaves your machine** unless you choose a
cloud model.

## Prerequisites

- **Rust** 1.82+ (stable) — [install via rustup](https://rustup.rs)
- **Ollama** — [install from ollama.com](https://ollama.com) (for local AI; skip if using OpenAI/Anthropic)
- **ffprobe** — for audio/video metadata (`brew install ffmpeg` on macOS, `apt install ffmpeg` on Linux)

> **macOS / Homebrew note:** install the official **Ollama app** (`brew install --cask ollama-app`,
> or the `.dmg` from ollama.com), **not** the `ollama` Homebrew *formula* — the formula currently
> ships without the model runtime, so `deep`/`summarize` fail with a missing-runner error. After
> installing, launch it once (`open -a Ollama`) so the server is listening on `:11434`.

## Install

```bash
# Clone and build
git clone https://github.com/harf-promo/indexa
cd indexa
cargo build --release

# Copy to your PATH (optional)
cp target/release/indexa /usr/local/bin/indexa
```

## Pull the models

```bash
ollama pull nomic-embed-text   # embedding model (~270MB, Apache-2.0)
ollama pull gemma3:4b          # file summaries (~2.5GB, Google)
ollama pull gemma3:12b         # answers + directory roll-ups (~8GB, Google)
```

All three run entirely locally — your data never leaves your machine. (`gemma3:4b` alone is enough to get started; `gemma3:12b` is used for answers and directory roll-up summaries.)

## Check your setup first

```bash
indexa doctor
```

`doctor` reports your machine's RAM/CPU, estimates per-model memory and job times, **probes
Ollama** (is it running? are the models you've configured actually pulled?), and ends with a
**Readiness** line — `✅ Ready to index` or a list of exactly what to fix first. Run it before
your first build so you don't discover a missing model halfway through.

## Index a folder

```bash
# One shot: scan → deep embed → summaries, in a single command
indexa index ~/Documents
```

Or run the phases separately to inspect each:

```bash
indexa scan ~/Documents   # surface scan — fast, no AI (builds the map)
indexa deep ~/Documents   # deep scan — parses, embeds, indexes content
indexa summarize ~/Documents  # hierarchical AI summaries
```

The deep + summarize phases take a few minutes for large folders. Progress is shown per-folder,
and the build pauses itself if the machine runs low on memory (see [`config.md`](config.md#resource-awareness)).

## Ask a question

```bash
indexa ask "where are my tax documents?"
indexa ask "show me Python files that use async/await"
indexa ask "what presentations did I work on last year?"
```

Useful flags:

```bash
indexa ask --json "…"      # structured {answer, sources} for scripts / CI
indexa ask --explain "…"   # print the retrieval trace (sparse vs dense vs fused hits) to debug results
indexa ask --agentic "…"   # multi-hop plan→search→refine for compositional questions
indexa status --json       # index stats as JSON
```

## Open the web UI

```bash
indexa serve
# → Open http://localhost:7620 in your browser
```

The web UI has a chat interface, search bar, and context tree sidebar.

## Keep the index current

```bash
indexa watch ~/Documents
```

The watcher re-indexes files as you create, edit, or delete them.

---

## Using a different AI model

Edit your config file (run `indexa status` to see its exact path — on macOS it's
`~/Library/Application Support/dev.indexa.Indexa/config.toml`, on Linux `~/.config/indexa/config.toml`):

```toml
# Use OpenAI for embeddings and Claude for answers
[embedding]
provider = "openai"
model    = "text-embedding-3-small"
dim      = 1536

[describer]
provider = "anthropic"
model    = "claude-sonnet-4-6"
```

Set the required environment variables:

```bash
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."
```

### Use your Claude subscription (no per-token billing)

If you have a Claude Pro/Max plan and the [Claude Code](https://claude.com/claude-code) CLI installed,
route **answer synthesis and summaries** through it — embeddings always stay local:

```toml
[describer]
provider = "claude-code"
model    = "claude-sonnet-4-6"
```

`indexa doctor` shows whether the `claude` CLI is found and signed in. The full embedding model
recommendations live in [`config.md`](config.md#recommended-embedding-models).

---

## Build context for your whole disk

```bash
indexa scan --all   # surface context map for everything
indexa deep /       # deep context — takes a while, start with a folder instead
```

The surface scan finishes in under a minute even on large disks. Building deep context for everything is slow — start with the folders that matter most, like `~/Documents` and `~/Projects`.

---

## Next steps

- [How-to guides](how-to/README.md) — export for Claude/Cursor, serve over MCP, debug the code
  graph, tune for a small machine
- [Configuration Reference](config.md) — all options documented (incl. a "when to tune retrieval" guide)
- [Architecture](architecture.md) — crate map and data flows
- [Indexing Methodology](methodology.md) — how the search pipeline works
- [Contributing](../CONTRIBUTING.md) — run tests, submit a PR

## Customizing the AI endpoint

Indexa respects environment variables so you don't need to edit config files:

```bash
# Point at a remote Ollama server
export OLLAMA_HOST=http://my-server:11434

# Use OpenAI instead
export OPENAI_API_KEY=sk-...
# then set provider = "openai" in config.toml

# Use Google Gemini for embeddings
export GOOGLE_API_KEY=AIza...
# then set provider = "google" in config.toml
```
