# Indexa

**The first tool to give your computer a memory.**

*Indexa reads every file and folder you point it at, understands what they are, and lets you ask your computer questions in your own words — all locally, with your own AI model.*

> Status: **pre-alpha** — foundations being built. Watch this repo or join [Discussions](../../discussions) to follow along.

---

## Why this is new

**No file search tool actually understands your files.**
Spotlight, Everything, and Recoll match keywords. They tell you a file *exists*, not what it *means*. Indexa reads your files the way a colleague would — it knows that `Q3_review_final_v2.docx` is a performance review for someone named Jordan, and that the folder called `random` is actually your photography archive from 2019.

**No AI tool indexes your whole machine.**
Chat-with-docs apps (AnythingLLM, PrivateGPT, and others) only know the folders you explicitly drop into them. Indexa builds a living memory of everything you own — documents, code, images, audio, video — and keeps it current as your files change.

**No private whole-disk tool is open.**
Apple Spotlight and Windows Recall do surface-level indexing behind proprietary code on locked-down platforms. Indexa is fully open source, runs on macOS, Linux, and Windows, and never sends your data anywhere unless you explicitly point it at a cloud model.

---

## Scope it your way

You don't have to index your whole computer on day one. Start with what matters.

```bash
# Index one folder
indexa scan ~/Documents

# Index several folders
indexa scan ~/Projects ~/Notes ~/Desktop

# Index the whole computer (uses a fast two-phase scan — see below)
indexa scan --all
```

Then ask questions in plain language:

```bash
indexa ask "where are my tax documents from last year?"
indexa ask "which of my code projects use Postgres?"
indexa ask "do I have any photos from the Morocco trip?"
```

Or open the local web UI for a visual map and chat:

```bash
indexa serve   # opens http://localhost:7620
```

---

## Free context for your paid AI coding tools

Claude Code, GitHub Copilot, Cursor, and Codex burn their context windows — and your paid tokens — just *understanding what's in your repo*. Indexa indexes the entire codebase locally with Ollama (free, offline), builds a hierarchical summary tree, and exports it in the format Anthropic's own docs recommend for LLM context windows:

```bash
indexa scan ~/code/my-monorepo
indexa summarize ~/code/my-monorepo
indexa export ~/code/my-monorepo --format xml > .context.xml
claude "given @.context.xml, find the auth flow and add MFA"
```

The paid model spends its budget on *the change you actually want* — not on re-reading your folder tree. **Zero tokens leave your machine during indexing.** You control exactly what gets handed to the cloud model.

---

## How it works

Indexa understands your files in two phases so you get value immediately, not after hours of processing.

**Phase 1 — Surface scan (seconds to minutes)**
Indexa walks your directory tree and builds a *map* of your computer: which regions are code projects, which are photo libraries, which are app data, which are build artifacts to skip. This phase makes zero AI calls and produces a visual treemap you can explore right away.

**Phase 2 — Deep scan (background, per region)**
For each region worth understanding, Indexa reads file content, extracts structure (code symbols, PDF text, image metadata), generates a description using your AI model of choice, and stores a vector embedding for semantic search. You can trigger this on-demand for a specific folder or let the background daemon work through your disk in priority order.

The result is a single index file at `~/.indexa/index.db` — one file, zero external services, easy to back up, easy to delete.

---

## Supported AI adapters

Bring your own model. No model is bundled — Indexa works with whatever you already have running. Configure in `~/.indexa/config.toml`:

| Adapter | How it runs |
|---|---|
| **Ollama** | Local, fully offline. Override server with `OLLAMA_HOST` env var. |
| **Google Gemini** | Cloud embeddings (`GOOGLE_API_KEY`). `text-embedding-004` matches local quality. |
| **llama.cpp** | Local via HTTP server. |
| **OpenAI** | Cloud — data leaves your device. `OPENAI_API_KEY` required. |
| **Anthropic** | Cloud — data leaves your device. `ANTHROPIC_API_KEY` required. |

Default models: `nomic-embed-text` (embedding, Ollama) · `gemma3:12b` (answers, Ollama, Google/Apache-2.0) · `gemma3:4b` (file summaries).

---

## Installation

Pre-built binaries for macOS (arm64, x86\_64), Linux (x86\_64, arm64), and Windows (x86\_64) will be available on the [Releases](../../releases) page when v0.1 ships.

Build from source (requires Rust ≥ 1.82):

```bash
git clone https://github.com/harf-promo/indexa
cd indexa
cargo build --release
# binary at target/release/indexa
```

---

## What's coming

Indexa is being built in the open. Here is what comes after the initial release, in rough order — no dates, ships when it's ready:

- **Software fingerprinting** — detect installed apps, frameworks, and project types by file patterns
- **Smart classification** — automatically suggest "this looks like a work directory / personal archive / media library"; you confirm or correct
- **Importance weighting** — tell Indexa which parts of your disk matter most; it adjusts search ranking accordingly
- **Insights** — duplicate file clusters, stale projects, weekly change reports
- **Mobile** — read-only companion app to browse your index from a phone
- **Plugin SDK** — extend Indexa with custom parsers, AI adapters, and insight modules

See [ROADMAP.md](ROADMAP.md) for detail. Vote on ideas and suggest new ones in [Discussions](../../discussions/categories/ideas).

---

## Contributing

Indexa is an early-stage project actively looking for contributors. All skill levels welcome.

- Read [CONTRIBUTING.md](CONTRIBUTING.md) for dev setup and PR process.
- Browse [`good first issue`](../../issues?q=label%3A%22good+first+issue%22) labels for scoped, newcomer-friendly work.
- Join the conversation in [Discussions](../../discussions).

Contributors sign off with the [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`). No CLA.

---

## License

Apache License 2.0 — see [LICENSE](LICENSE).

Copyright 2025 Harf Promo.
