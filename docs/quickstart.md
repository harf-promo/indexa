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

## Pull the embedding model

```bash
ollama pull nomic-embed-text
```

This pulls a ~270MB model that runs locally. It never sends data anywhere.

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

The web UI has a chat interface, search bar, and disk map sidebar.

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

## Index your whole computer

```bash
indexa scan --all   # surface scan everything
indexa deep /       # deep scan (takes a while — start with a folder instead)
```

The surface scan finishes in under a minute even on large disks. Deep scanning everything is slow — start with the folders that matter most, like `~/Documents` and `~/Projects`.

---

## Next steps

- [Configuration Reference](config.md) — all options documented
- [Indexing Methodology](methodology.md) — how the search pipeline works
- [Contributing](../CONTRIBUTING.md) — run tests, submit a PR
