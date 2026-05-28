# Quickstart

Get Indexa running in under 5 minutes.

## Prerequisites

- **Rust** 1.75+ — [install via rustup](https://rustup.rs)
- **Ollama** — [install from ollama.com](https://ollama.com) (for local AI; skip if using OpenAI/Anthropic)
- **ffprobe** — for audio/video metadata (`brew install ffmpeg` on macOS, `apt install ffmpeg` on Linux)

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
ollama pull gemma2:9b          # answer model (~5GB, Google, Apache-2.0)
```

Both run entirely locally — your data never leaves your machine.

## Index a folder

```bash
# Surface scan — fast, no AI (builds the map)
indexa scan ~/Documents

# Deep scan — parses, embeds, indexes content
indexa deep ~/Documents
```

The deep scan will take a few minutes for large folders. Progress is shown per-folder.

## Ask a question

```bash
indexa ask "where are my tax documents?"
indexa ask "show me Python files that use async/await"
indexa ask "what presentations did I work on last year?"
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

Create `~/.indexa/config.toml`:

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

---

## Build context for your whole disk

```bash
indexa scan --all   # surface context map for everything
indexa deep /       # deep context — takes a while, start with a folder instead
```

The surface scan finishes in under a minute even on large disks. Building deep context for everything is slow — start with the folders that matter most, like `~/Documents` and `~/Projects`.

---

## Next steps

- [Configuration Reference](config.md) — all options documented
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
