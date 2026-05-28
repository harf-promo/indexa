# Indexa

**The local context engine for AI.**

*Indexa reads your code or your disk once, builds a hierarchical context graph, and serves it to AI tools without burning their token budgets. Local-first, model-agnostic, fully open.*

> Status: **pre-alpha** — foundations being built. Watch this repo or join [Discussions](../../discussions) to follow along.

---

## How it fits in your AI workflow

Claude Code, GitHub Copilot, Cursor, and Codex burn their context windows — and your paid tokens — just *understanding what's in your repo*. Indexa builds that context locally with Ollama (free, offline), and exports it in the format Anthropic's own docs recommend for LLM context windows:

```bash
indexa scan ~/code/my-monorepo
indexa summarize ~/code/my-monorepo
indexa export ~/code/my-monorepo --format xml > .context.xml
claude "given @.context.xml, find the auth flow and add MFA"
```

The paid model spends its budget on *the change you actually want* — not on re-reading your folder tree. **Zero tokens leave your machine during indexing.** You control exactly what gets handed to the cloud model.

---

## Why this is different

**Paid AI tools burn context re-learning your repo every session.**
Claude Code, Copilot, and Cursor see your code for the first time on every chat. They spend tokens (and latency) just orienting themselves. Indexa builds a persistent, grounded understanding of your codebase once — locally — and makes it available on demand.

**Existing "AI knowledge base" tools are SaaS, opaque, or both.**
AnythingLLM, PrivateGPT, and similar tools require you to explicitly drop folders into them. Indexa indexes everything you point it at — documents, code, images, audio, video — and keeps context current as files change.

**Your data stays on your hardware.**
Indexa runs fully offline with Ollama. It is fully open source, runs on macOS, Linux, and Windows, and never sends your data anywhere unless you explicitly point it at a cloud model.

---

## Scope it your way

You don't have to index your whole computer on day one. Start with what matters.

```bash
# Build context for one folder
indexa scan ~/Documents

# Build context for several folders
indexa scan ~/Projects ~/Notes ~/Desktop

# Build context for the whole computer
indexa scan --all
```

Then ask questions in plain language:

```bash
indexa ask "where are my tax documents from last year?"
indexa ask "which of my code projects use Postgres?"
indexa ask "where is auth handled in this repo?"
```

Or open the local web UI for a visual context tree and chat:

```bash
indexa serve   # opens http://localhost:7620
```

---

## How it works

Indexa builds context in two phases so you get value immediately, not after hours of processing.

**Phase 1 — Context map (seconds to minutes)**
Indexa walks your directory tree and builds a *context map*: which regions are code projects, which are photo libraries, which are app data, which are build artifacts to skip. This phase makes zero AI calls and produces a visual context tree you can explore right away.

**Phase 2 — Deep context (background, per region)**
For each region worth understanding, Indexa reads file content, extracts structure (code symbols, PDF text, image metadata), generates a per-file context summary using your AI model of choice, and rolls summaries up into folder-level context. The result is a hierarchical context graph you can export and hand to any AI tool.

The entire context store lives in a single file at `~/.indexa/index.db` — one file, zero external services, easy to back up, easy to delete.

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

Default models: `nomic-embed-text` (embedding, Ollama) · `gemma3:12b` (answers + dir context, Ollama, Google/Apache-2.0) · `gemma3:4b` (file context summaries).

---

## Installation

Download a pre-built binary from the [Releases](../../releases) page:

```bash
# macOS (Apple Silicon)
curl -L -o /usr/local/bin/indexa \
  https://github.com/harf-promo/indexa/releases/latest/download/indexa-aarch64-apple-darwin
chmod +x /usr/local/bin/indexa
xattr -d com.apple.quarantine /usr/local/bin/indexa   # bypass Gatekeeper if prompted

# macOS (Intel)
curl -L -o /usr/local/bin/indexa \
  https://github.com/harf-promo/indexa/releases/latest/download/indexa-x86_64-apple-darwin
chmod +x /usr/local/bin/indexa

# Linux x86_64
curl -L -o /usr/local/bin/indexa \
  https://github.com/harf-promo/indexa/releases/latest/download/indexa-x86_64-linux-gnu
chmod +x /usr/local/bin/indexa
```

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

- **Software fingerprinting** — detect installed apps, frameworks, and project types by file patterns; surface them as context metadata
- **Smart context tagging** — automatically classify regions as "active work / archive / media / code / system"; you confirm or correct
- **Importance weighting** — tell Indexa which parts of your context store matter most; it adjusts retrieval ranking accordingly
- **Insights** — duplicate file clusters, stale projects, weekly change reports
- **Mobile** — read-only companion app to query your context store from a phone
- **Plugin SDK** — extend Indexa with custom parsers, AI adapters, and context modules

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
