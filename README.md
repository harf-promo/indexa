# Indexa

**The local context engine for AI.**

Your AI meets your codebase cold on every chat — burning paid tokens to relearn what it knew yesterday, or choking on a context window that can't hold your repo at all. Indexa reads your code or your whole disk **once, on your machine**, and hands any AI tool the small, relevant slice it needs — not the whole tree.

```bash
indexa scan      ~/code/my-repo            # instant context map — zero AI
indexa deep      ~/code/my-repo            # parse + embed, fully local
indexa summarize ~/code/my-repo            # hierarchical L0/L1/L2 context
indexa export    ~/code/my-repo --format xml > .context.xml
claude "given @.context.xml, find the auth flow and add MFA"
# the paid model spends its budget on the change — not on re-reading your tree
```

**For cloud agents** (Claude Code · Cursor · Copilot · Codex): stop paying tokens to re-read your tree.
**For local models** (Ollama · llama.cpp): reason over a 100 MB repo from a 4K window.

*The index is the substrate; context is the product. Local-first · model-agnostic · Apache-2.0.*

> **Status — v0.12.3.** Thirteen releases of shipped features, built in the open. The full flow works today — scan → deep context → hierarchical summaries → ask, plus a local web workspace, a native desktop app (macOS), and an MCP server for AI agents. A daily-driver for a single repo; very large whole-disk runs are still getting faster, and the storage format may move before 1.0. New here? Start with the **[Usage Guide](USAGE.md)** or **[Quickstart](docs/quickstart.md)**.

---

## See it work

Three commands build the context. The fourth uses it.

```console
$ indexa scan ~/code/my-repo
Scanning ~/code/my-repo
  1,284 entries
Index saved to ~/.indexa/index.db
Run `indexa map` to see a summary.
Run `indexa deep <path>` to parse and embed file contents.

$ indexa deep ~/code/my-repo          # parse + embed, fully local — nothing leaves the machine
  embedded 6,470 new chunks.

$ indexa summarize ~/code/my-repo     # 1–2 sentence summary per file, rolled up folder-by-folder
Done. 318 summaries generated.

$ indexa ask "where is auth handled?"
Searching 6,470 indexed chunks...

Answer:
Authentication is handled in src/auth/middleware.rs (the `require_auth` route
guard) and src/auth/login.rs (the `login` entry point). Session tokens are
minted and validated in src/auth/session.rs. [1, 2, 3]

Sources:
  [1] src/auth/middleware.rs — require_auth
  [2] src/auth/login.rs — login
  [3] src/auth/session.rs — mint_token
```

Then hand it to your AI tool — or skip the file entirely and let your agent pull context live over MCP:

```bash
indexa export ~/code/my-repo --format xml > .context.xml   # the artifact, built on your machine
indexa serve                                               # or open the web workspace at :7620
indexa mcp                                                 # or expose the live index to any agent
```

---

## Why Indexa

**Stop paying to re-teach your AI your own codebase.** Every coding assistant wakes up amnesiac. Before it helps, it reads its way back to orientation — burning context window, paid tokens, and your patience on a lesson it learned five minutes ago. Indexa teaches it *once*: it builds a persistent, hierarchical context store on your machine and serves a small ranked slice on demand, so the model spends its budget on the work you asked for.

**There are two kinds of context, and almost everyone conflates them.** *Working context* is what's in the model's window right now — scarce, paid, gone when the session ends. *Searchable context* is everything your AI could know: the store on disk. Indexa separates them. The model never holds your repo; it holds the ~2–4K characters that actually matter, retrieved from a store that can be gigabytes.

**Local isn't the compromise — it's the unlock.** Your data never leaves the machine unless *you* point it at a cloud model, and zero tokens leave while Indexa builds context. A small local model stops being small: feed it a retrieved slice instead of the whole project and a 4K-window model reasons over a 100 MB codebase — fast, even on CPU, with a KV-cache sized by your choice, not your repo. One engine, two wins: it saves cloud tools their tokens **and** gives local models the context they can't hold.

*(How retrieval keeps that slice relevant — hybrid search, the ANN index, the honest trade-offs — is in [docs/methodology.md](docs/methodology.md).)*

---

## What ships today (v0.12.3)

- **Two-phase context** — an instant surface scan (zero AI, classifies code vs media vs build artifacts) then deep context: parse → chunk → embed → LLM file summaries rolled up bottom-up into a hierarchical context graph, with **L0 / L1 / L2 progressive disclosure** (one-line abstract → full summary → raw content).
- **Hybrid retrieval** — keyword (BM25) + semantic (vector) fused with RRF, plus an **opt-in ANN index** that keeps dense search fast on 50K-plus-chunk corpora.
- **Local multimodal** *(opt-in, on-device)* — caption images with a local vision model and transcribe audio with a local whisper CLI, so you can find media by what's *in* it, not just its filename.
- **Code intelligence** — a code-relationship graph (imports + defined symbols + call edges) across Rust, Python, JS/TS, Go, and Java, plus `who_calls` and `blast_radius` for impact analysis — all queryable over MCP.
- **Four interfaces** — a CLI, a local web workspace with a live **Engine** status bar, a **native macOS desktop app** (auto-updating, menu-bar tray), and a native **MCP** server (10 tools) for AI agents.
- **Resource-aware** — a memory watchdog that won't freeze your machine, and a hardware-aware model picker that annotates every model with its memory footprint, fit against your live RAM, and a per-job ETA.
- **Use your Claude subscription** — the `claude-code` provider runs summaries and answers on your Claude Pro/Max plan (no per-token billing); embeddings always stay local.
- **Export** — XML (the format Anthropic's own docs recommend for context windows), Markdown, or JSON. **Watch** keeps the context current as files change.

---

## Three ways in

- **CLI** — `scan · deep · summarize · ask · export · watch · serve · mcp · doctor · classify · update`. Scriptable, pipeable, zero services.
- **Web workspace** — `indexa serve` → `http://localhost:7620`. A live Engine status bar shows what the machine is doing while it builds:
  ```
  Engine  Building · 42 files/s · ETA 1m12s · gemma3:4b    CPU 38%   RAM 9.1 / 16 GB   pressure: ok
  ```
- **Desktop app** — a native macOS app (Apple Silicon; Intel via CLI) that lives in the menu bar. Auto-updates silently. Bundles the web workspace — no separate `indexa serve` needed.
- **MCP server** — `indexa mcp` exposes the live index to any [MCP](https://modelcontextprotocol.io) client (Claude Desktop, Cursor, Claude Code) over **10 tools**:
  `search · browse_tree · get_summary · read_file · ask · dependencies · who_imports · who_calls · blast_radius · get_stats`.

---

## Code intelligence

Deep-indexing records each code file's graph edges — what it **imports** and the symbols it **defines** — across Rust, Python, JS/TS, Go, and Java. Two MCP tools query it, so your agent reasons about structure without reading every file:

```text
dependencies("src/auth/session.rs")
  → imports:  crate::store::Db
  → defines:  Session, mint_token, validate

who_imports("crate::store::Db")
  → src/auth/session.rs
```

---

## Why it's defensible

No tool occupies all of this at once:

| | Local / offline | Whole-disk **and** code | Persistent index + retrieval | CLI · Web · MCP |
|---|---|---|---|---|
| **Indexa** | ✅ | ✅ | ✅ | ✅ |
| Repomix / gitingest (repo→prompt) | ✅ | repo only | ❌ one-shot | CLI |
| AnythingLLM / Khoj (local doc-chat) | ✅ | manual docs | ✅ | desktop/web |
| Continue / Cursor / Cody (IDE) | ✅ / cloud | repo | partial | IDE |
| Codebase-graph skills (graphify, etc.) | cloud LLM | folder | regenerated per run | skill/plugin |

That intersection — local-first, whole-disk *and* code, a persistent queryable store, three interfaces, resource-aware, and valuable to **both** cloud and local models — is where Indexa lives. Full breakdown in [docs/COMPETITIVE.md](docs/COMPETITIVE.md).

---

## Install

Download a pre-built binary from [Releases](../../releases):

```bash
# macOS (Apple Silicon)
curl -L -o /usr/local/bin/indexa \
  https://github.com/harf-promo/indexa/releases/latest/download/indexa-aarch64-apple-darwin
chmod +x /usr/local/bin/indexa
xattr -d com.apple.quarantine /usr/local/bin/indexa   # bypass Gatekeeper if prompted

# macOS (Intel): indexa-x86_64-apple-darwin
# Linux x86_64:  indexa-x86_64-linux-gnu      ·  Linux arm64: indexa-aarch64-linux-gnu
# Windows x64:   indexa-x86_64-windows.exe
```

Pull the local models (one-time, ~11 GB total; everything runs offline after this):

```bash
ollama pull nomic-embed-text   # embeddings (~270 MB)
ollama pull gemma3:4b          # file summaries (~2.5 GB)
ollama pull gemma3:12b         # answers + directory roll-ups (~8 GB)
```

Or build from source (Rust ≥ 1.82): `git clone … && cargo build --release` → `target/release/indexa`.

---

## Bring your own model

No model is bundled — Indexa works with whatever you run. Configure in `~/.indexa/config.toml`:

| Adapter | How it runs |
|---|---|
| **Ollama** | Local, fully offline (default). Point elsewhere with `OLLAMA_HOST`. |
| **Claude subscription** | `provider = "claude-code"` — synthesis on your Claude Pro/Max plan, no per-token billing. Embeddings stay local. |
| **llama.cpp** | Local via its HTTP server. |
| **Google Gemini · OpenAI · Anthropic** | Cloud — data leaves your device; API key required. |

Defaults: `nomic-embed-text` (embeddings) · `gemma3:4b` (file context) · `gemma3:12b` (answers + roll-ups). Optional cross-encoder reranking *fails open* — a model hiccup falls back to the original order, so it can never make `ask` worse.

---

## Roadmap

Built in the open; ships when ready. **Coming, not yet shipped:**

- **Context Packs** — auto-detect files scattered across your disk that belong to one subject ("Auth", "Tax 2025", "Client X"), bundle them into a named context, and export as one portable file.
- **Context-Map visualization** — the folder tree as a coverage-colored treemap/sunburst.
- **Deeper code graph** — cross-file call edges and "what breaks if I change this?" blast-radius queries.
- **Desktop app · Plugin SDK** — a background service with menu-bar control; custom parsers and adapters.

See [ROADMAP.md](ROADMAP.md); vote on ideas in [Discussions](../../discussions/categories/ideas).

---

## Contributing

Indexa is built in the open and welcomes contributors of every level. Read [CONTRIBUTING.md](CONTRIBUTING.md), browse [`good first issue`](../../issues?q=label%3A%22good+first+issue%22), and join [Discussions](../../discussions). Commits sign off with the [DCO](https://developercertificate.org/) (`git commit -s`); no CLA.

## License

Apache License 2.0 — see [LICENSE](LICENSE). Copyright 2025 Harf Promo.
