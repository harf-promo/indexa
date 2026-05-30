# Indexa

**The local context engine for AI.**

*Indexa reads your code or your disk once, builds a hierarchical context graph, and serves any AI tool a small, relevant slice on demand — without burning a cloud model's token budget or a local model's context window. Local-first, model-agnostic, fully open.*

> Status: **v0.5.0**, pre-1.0 and actively developed. The scan → summarize → export → ask flow works today, plus a local web UI and an MCP server for AI agents. Expect rough edges on large / whole-disk indexes. Watch this repo or join [Discussions](../../discussions) to follow along.

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

Or skip the file hand-off entirely: run `indexa mcp` and your agent (Claude Desktop, Cursor, any [MCP](https://modelcontextprotocol.io) client) queries the live index over six tools — `search`, `browse_tree`, `get_summary` (with L0/L1/L2 progressive disclosure), `read_file`, `ask`, and `get_stats`.

---

## Punch above your local model's context window

Running a small model locally with Ollama or llama.cpp? Context is your scarcest resource. Every token you stuff into the window inflates the KV-cache that competes with the model's own weights for VRAM — a long prompt is slow to start and can push an 8 GB machine into swap. Most local models ship a 4–8K-token window; paste in a whole repo and there's no room left to think.

Indexa separates your **working context** (what's in the model's window right now) from your **searchable context** (the persistent index on disk). Your model only ever sees a small, ranked slice of what's actually relevant:

- **Bounded memory** — retrieve ~2–4K characters instead of a 600 MB repo, so the KV-cache stays small and predictable, sized by *your* choice, not your codebase.
- **Break the window ceiling** — a 4K-window model can reason over a 100 MB project, because it only loads the slice that matters.
- **Fast, even on CPU** — small context means fast prefill and a snappy time-to-first-token.
- **Agents that don't forget** — over MCP, a local agent pulls `get_summary("auth")` on demand instead of pre-loading everything, staying coherent across long multi-step tasks without hitting the memory cliff.

Same engine, two wins: it saves **cloud** tools your paid tokens, and gives **local** models the context they can't hold themselves. *(How retrieval keeps that slice relevant — hybrid search, reranking, and the honest trade-offs — is in [docs/methodology.md](docs/methodology.md).)*

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

**Optional reranking.** Set `rerank = true` under `[retrieval]` to add a cross-encoder pass that reorders retrieved candidates with one local-model call before the answer is synthesized. It's off by default and *fails open* — any model hiccup falls back to the original order, so it can never make `ask` worse.

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

**Already shipped** (v0.2–v0.5): hierarchical summarization with tiered L0/L1/L2 abstracts, the local web UI, resource-aware indexing (won't freeze your machine), an MCP server for AI agents, and optional cross-encoder reranking.

Indexa is being built in the open. Here is what comes next, in rough order — no dates, ships when it's ready:

- **Software fingerprinting** — detect installed apps, frameworks, and project types by file patterns; surface them as context metadata
- **Smart context tagging** — automatically classify regions as "active work / archive / media / code / system"; you confirm or correct
- **Importance weighting** — tell Indexa which parts of your context store matter most; it adjusts retrieval ranking accordingly
- **Context Packs** — auto-detect files and folders scattered across your disk that all belong to one subject ("Auth", "Tax 2025", "Client X"), bundle them into a named context, and export it as a single portable file (XML/Markdown) to hand to any AI tool — or a teammate
- **Insights** — duplicate file clusters, stale projects, weekly change reports
- **Desktop app** — a native, installable build (Tauri) that runs as a background service with menu-bar control and a signed installer — no terminal left open
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
