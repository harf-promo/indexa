# Troubleshooting

Start every investigation the same way:

```bash
indexa doctor
```

It profiles the machine (RAM, Apple Silicon unified memory), probes the Ollama server and each
configured model, checks index integrity, and prints a **Readiness** verdict with the exact fix for
anything that fails. Most of the issues below end with "doctor would have told you this."

---

## Ollama: "not reachable" / model errors

**Symptom:** `deep`, `summarize`, or `ask` fail immediately; doctor's *Ollama server (liveness)*
section reports the server or a model missing.

- Indexa expects Ollama at `http://localhost:11434` (override with the `OLLAMA_HOST` env var, or
  `base_url` in the config's `[embedding]` / `[llm]` sections).
- The default models must be pulled once:

  ```bash
  ollama pull nomic-embed-text   # embedding (~270 MB)
  ollama pull gemma3:4b          # file summaries (~2.5 GB)
  ollama pull gemma3:12b         # dir roll-ups + Q&A (~8 GB)
  ollama list                    # verify
  ```

- On macOS, if you installed the **Ollama app**, the server starts when the app does — check the
  menu bar. (The Homebrew *formula* installs a service instead; don't assume one because you have
  the other.)

## Indexing stalls with "memory pressure" / "waiting for swap"

**Symptom:** the Engine bar (or worker log) shows the watchdog easing off, and progress pauses.

This is the memory watchdog doing its job: it throttles or pauses indexing rather than letting a
local model freeze the machine. It resumes when pressure clears. If it happens constantly:

- Run `indexa doctor` — the *Why Indexa can freeze the machine* section shows your live budget and
  which configured model doesn't fit.
- Leave `[resource] auto_select_model = true` (the default): summarize/worker pre-flight the model
  fit and downgrade the roll-up model (e.g. `gemma3:12b` → `gemma3:4b`) when it wouldn't fit.
- On a tight machine, follow [Tune Indexa for a small machine](how-to/tune-for-a-small-machine.md)
  (conservative profile, summaries-only mode).
- Doctor also prints recommended Ollama server settings (`OLLAMA_MAX_LOADED_MODELS=1`,
  `OLLAMA_NUM_PARALLEL=1`, `OLLAMA_KEEP_ALIVE=30s`) that stop Ollama holding multiple models
  resident.

## Search finds nothing (or misses things it used to find)

- **Semantic search returns nothing at all** → the chunks probably have no embeddings (e.g. Ollama
  was down during `deep`). Re-run `indexa deep <path>`: since v0.20 it re-embeds any file whose
  chunks are missing vectors, so a plain re-run heals the index.
- **You switched embedding models** → old and new vectors aren't comparable. Re-run `deep` over
  your roots so everything is embedded by the same model (the web UI warns about exactly this when
  you change the embedder).
- **You edited files and answers are stale** → re-run `indexa deep` (it compares against live
  on-disk mtime), or keep `indexa watch` running so changes are picked up as they happen.
- **A whole directory is missing** → check it isn't excluded by `.gitignore` (honored by default
  since v0.20; `[scan] respect_gitignore = false` to disable) or the `[scan] ignore` list.

## Scanned PDFs / images produce empty results

Image-only PDFs have no extractable text — they produce empty chunks (OCR is a planned opt-in, not
shipped). For images and audio, enable the opt-in **local multimodal** parsers (vision captioning,
whisper transcription) in the config; otherwise media is indexed by metadata only.

## The summary queue looks stuck

```bash
indexa status            # queue depth by state
indexa worker            # drain pending summaries in the background
```

`failed` rows are retried with backoff. If `pending` never moves, doctor's liveness probe will tell
you whether the summarization model is the problem.

## Web UI

- `indexa serve` listens on **http://localhost:7620**. Use `--host 0.0.0.0` to reach it from
  another device on your LAN.
- Editing API keys from the browser requires `INDEXA_WEB_ALLOW_KEY_EDIT=1` in the server's
  environment (deliberate safety gate; keys are written `0600` and never logged).

## macOS-specific

- **App or CLI killed right after a self-update** (exit 137): macOS's code-signing monitor kills a
  binary replaced in place. Fixed in v0.17 (CLI) and v0.19 (desktop) — both re-sign after
  updating. If you're on an older version, one manual reinstall gets you current.
- **v0.20.0 desktop app crashes instantly at launch**: that release was withdrawn (a dynamically
  linked Homebrew `libpcre2` was rejected by the hardened runtime). Install
  [v0.20.1](https://github.com/harf-promo/indexa/releases/tag/v0.20.1) manually; auto-update
  resumes from there.

## Where is my data? / How do I start over?

The whole index is one SQLite file — see the per-platform path table in [USAGE.md](../USAGE.md)
(§2, "The mental model"). To remove one root from the index, `indexa rm <path>`; to GC orphaned rows
after removing roots, `indexa prune`; to start completely fresh, stop Indexa and delete `index.db`
(your files on disk are never touched — the index is derived data).
