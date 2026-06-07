# Tune Indexa for a small machine

**Goal:** build context on an 8–16 GB laptop without the machine freezing or swapping itself to a
crawl. Indexa already pauses local-LLM work under memory pressure, but a few settings make the
experience smooth instead of stop-start.

## 1. See where you stand

```bash
indexa doctor
```

Read the **per-model memory table** and the **Budget** line. If `gemma3:12b` shows ❌ (doesn't fit),
you'll want a smaller describer model and/or the conservative profile below.

## 2. Pick a conservative resource profile

```toml
[resource]
profile = "conservative"   # largest memory headroom, shortest model keep-alive
```

| Profile | Use when |
|---|---|
| `conservative` | 8–16 GB RAM, or the machine must stay responsive for other work |
| `balanced` | **default** — typical laptop |
| `performance` | 32 GB+ and you want maximum throughput |

The watchdog reads RAM/swap before each model call and **eases off automatically** under pressure
(freeing the resident model so RAM recovers), then resumes — so a build slows down rather than
freezing the machine. See [`config.md`](../config.md#resource-awareness).

## 3. Use smaller models for summaries

`gemma3:4b` for everything keeps the resident footprint low. In config:

```toml
[describer]
model      = "gemma3:4b"   # answers
file_model = "gemma3:4b"   # per-file summaries
dir_model  = "gemma3:4b"   # directory roll-ups (gemma3:12b is better but heavier)
```

Embeddings (`nomic-embed-text`, ~270 MB) are tiny and stay local regardless.

## 4. Keep the context window bounded

```toml
[describer]
num_ctx = 4096   # default — keeps the KV-cache small (Ollama otherwise loads at 32k)
```

`num_ctx` is the single biggest lever on memory: the KV-cache grows with it. 4096 is the default for
exactly this reason; raising it is what most often pushes a small machine into swap.

## 5. Tell Ollama to unload promptly

Ollama keeps each model warm for 5 minutes by default; stacking models is what blows past RAM. On
macOS:

```bash
launchctl setenv OLLAMA_MAX_LOADED_MODELS 1   # one model resident at a time
launchctl setenv OLLAMA_NUM_PARALLEL 1        # no KV-cache multiplication
launchctl setenv OLLAMA_KEEP_ALIVE 30s        # unload between jobs
# then quit and relaunch Ollama.app
```

`indexa doctor` checks these env vars and flags any that aren't set.

## 6. Index in chunks, not all at once

Build the folders that matter first (`indexa index ~/code/active-project`) rather than `indexa deep /`.
Re-runs are incremental, so you can grow the index over several sessions.
