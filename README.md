# Indexa

**The open index for your whole computer.**

> Status: **pre-alpha** — foundations being built. Watch this repo or join [Discussions](../../discussions) to follow along.

Indexa walks every file and folder on your device, builds a rich semantic understanding of each one using a language model of your choice, and lets you search and ask questions in plain language — completely locally, with no data leaving your machine unless you configure a cloud model yourself.

```
indexa scan ~/Documents
indexa ask "where are my tax documents from last year?"
indexa serve   # opens http://localhost:7620 — folder graph + chat UI
```

---

## Why Indexa?

| | Spotlight / Windows Recall | AnythingLLM / PrivateGPT | Recoll / DocFetcher | **Indexa** |
|---|---|---|---|---|
| Indexes whole disk | ✅ | ❌ (folder you point at) | ✅ | ✅ |
| Open source | ❌ | ✅ | ✅ | ✅ |
| BYO LLM | ❌ | ✅ | ❌ | ✅ |
| Hierarchical folder graph | ❌ | ❌ | ❌ | ✅ |
| Cross-platform | ❌ | ✅ | ✅ | ✅ |

---

## How it works

Indexa understands files from the bottom up:

1. **Parse** — extract text, metadata, EXIF, code structure, PDF content, audio/video metadata.
2. **Describe** — ask a language model to summarize each file in one or two sentences.
3. **Embed** — store a vector representation for semantic search.
4. **Compose** — roll up understanding from files → folders → device, so you can ask about any level.
5. **Watch** — a background daemon tracks changes and keeps the index current.

Everything is stored in a single SQLite database at `~/.indexa/index.db`. Nothing else touches your filesystem.

---

## Supported LLM adapters (v0.1)

Bring your own model. Configure in `~/.indexa/config.toml`:

- **Ollama** (local, recommended — fully offline)
- **llama.cpp** HTTP server
- **OpenAI** (gpt-4o, text-embedding-3-small, …)
- **Anthropic** (claude-3-5-haiku, claude-opus-4, …)

---

## Installation

Pre-built binaries for macOS (arm64, x86\_64), Linux (x86\_64, arm64), and Windows (x86\_64) are available on the [Releases](../../releases) page once v0.1 ships.

Build from source (requires Rust ≥ 1.82):

```bash
git clone https://github.com/harf-promo/indexa
cd indexa
cargo build --release
# binary at target/release/indexa
```

---

## Roadmap

| Version | Focus |
|---|---|
| **v0.1** | Scan, watch, ask, serve. Four LLM adapters. Cross-platform binaries. |
| v0.2 | Software fingerprinting — detect installed apps and project types by file patterns. |
| v0.3 | Insights — duplicate clusters, stale projects, anomaly hints. |
| v0.4 | Mobile read-only view. |
| v0.5 | Plugin SDK for custom parsers, adapters, and insights. |

See [ROADMAP.md](ROADMAP.md) for detail.

---

## Contributing

Indexa is an early-stage project actively looking for contributors. All skill levels welcome.

- Read [CONTRIBUTING.md](CONTRIBUTING.md) for dev setup and PR process.
- Browse [Issues](../../issues) — items labeled [`good first issue`](../../issues?q=label%3A%22good+first+issue%22) are explicitly scoped for new contributors.
- Join the conversation in [Discussions](../../discussions).

Contributors sign off their commits using the [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`). No CLA required.

---

## License

Apache License 2.0 — see [LICENSE](LICENSE).

Copyright 2025 Harf Promo.
