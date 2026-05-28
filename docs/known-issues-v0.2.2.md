# Known issues — v0.2.2

Observed during end-to-end testing on 2026-05-28.  
All issues below were fixed in **v0.2.3** (branch `feat/v0.2.3-context-quality`).  
Issue tracker: [GitHub issue #33](https://github.com/harf-promo/indexa/issues/33).

---

## 1. `indexa --version` reports `0.1.0`

**Observed:** `/tmp/indexa --version` prints `indexa 0.1.0` on the v0.2.2 binary.

**Root cause:** `Cargo.toml` (workspace root, line 15) has `version = "0.1.0"`. All crates inherit `version.workspace = true`. The git tag (`v0.2.2`) and the binary version string are out of sync — the workspace version was never bumped.

**Fix:** Bump `Cargo.toml:15` to the current release version whenever a release tag is created. Consider automating via `cargo-release` or a release workflow step that patches `version` to match the tag.

**Fixed in v0.2.3:** Workspace `Cargo.toml` `version` bumped to `0.2.3`.

---

## 2. `indexa deep` re-embeds all chunks on every run

**Observed:**  
- First run: 5 files → 33 chunks → 1.4 s total  
- Second run (no file changes): 5 files → 33 chunks → 0.6 s total  
- Both runs sent all 33 embedding requests to Ollama/nomic-embed-text.

The only reason the second run was faster is that the embedding model was already warm in Ollama's memory. No chunks were skipped.

**Root cause:** `cmd_deep` in `apps/indexa/src/main.rs:186-215` walks every file, calls `embedder.embed(&chunk.text)` for every chunk unconditionally, and then calls `Store::upsert_chunks`. `upsert_chunks` in `crates/core/src/store.rs:180-222` is `INSERT OR REPLACE INTO chunks` keyed on `(entry_path, seq)` — it blindly replaces existing rows.

**Impact:**  
- Wastes embedding API calls / Ollama GPU time proportional to corpus size.  
- A 300 K-chunk index costs the same to re-deep as the initial scan.  
- With a remote embedder (OpenAI, Google), every re-deep is a paid API call.

**Expected fix shape:**  
Before embedding a chunk, check whether a row already exists in `chunks` with the same `(entry_path, seq)` AND the content hash (or `text` fingerprint) hasn't changed AND the `embed_model` matches. If all three hold, skip the embedding call and leave the row alone. A practical shortcut: skip if file `modified_s` in `entries` equals the stored timestamp at scan time and `embed_model` matches.

**Fixed in v0.2.3.**

---

## 3. Deleted files stay in the index (ghost rows)

**Observed:**  
Moved `crates/embed/src/google.rs` to `/tmp`, re-ran `indexa scan`, then queried the database directly:
```sql
SELECT path FROM entries WHERE path LIKE '%indexa/crates/embed%';
```
Result included `/…/crates/embed/src/google.rs` even though the file no longer existed on disk.

**Root cause:** `cmd_scan` in `apps/indexa/src/main.rs:88-105` calls `store.upsert_entries(&entries)` which is `INSERT OR REPLACE` — it updates or adds rows but never removes rows whose paths no longer appear in the walk result. The `delete_entry`, `delete_subtree`, and `delete_chunks_for` functions exist (`crates/core/src/store.rs:225-270`) and are used by `cmd_rm` and `cmd_watch`'s `Remove` handler, but `cmd_scan` never calls them.

**Impact:**  
- The index silently accumulates stale entries for moved/deleted files.  
- `indexa map` over-counts sizes and file counts.  
- `indexa ask` can surface irrelevant chunks from files that no longer exist.  
- Summary queue retains dead paths and will retry them on every `indexa worker` run.

**Expected fix shape:**  
After `walk()` produces the new entry set, compute the set difference against the currently stored paths under the same root(s), and call `store.delete_entry` (or a batch delete) for each removed path. Alternatively, do this as a reconcile pass: `DELETE FROM entries WHERE path LIKE '<root>/%' AND path NOT IN (<new paths>)`.

**Fixed in v0.2.3.**

---

## 4. `indexa summarize` falsely reports success when the model is missing

**Observed:**  
With `gemma3:4b` not installed in Ollama, running `indexa summarize <path>` produced:
```
Summarizing … 
Enqueued 0 items for summarization.
[WARN] summarize failed for …/google.rs: Ollama returned 404 Not Found: {"error":"model 'gemma3:4b' not found"}
[WARN] … (5 more identical WARNs, one per file)
Done. 7 summaries generated.
  7 summaries written.
```
The terminal output claimed 7 successes. The queue database shows 5 `failed` rows and 2 `done` (empty directory roll-ups). No summaries were actually generated for any file.

WARN messages are invisible unless `RUST_LOG=warn` is set; the default `INFO` filter hides them.

**Root cause:**  
In `apps/indexa/src/main.rs:600-609`, `summarize_subtree_sync` returns a count that is not differentiated by success vs failure. The count includes failed items. Error messages go to `tracing::warn!` which is filtered at `INFO` level by default.

**Impact:**  
A user upgrading from v0.1 to v0.2 (new Gemma 3 defaults) runs `indexa summarize` without pulling `gemma3:4b` first. The output tells them it worked. They run `indexa ask` and get poor results. There is no indication of what went wrong.

**Expected fix shape:**  
- Emit a user-visible `eprintln!("error: …")` for each failed item (or a summary: "5 items failed — run `ollama pull gemma3:4b` to install the required model").  
- Return a non-zero exit code when any item failed.  
- Consider adding a `--check-models` pre-flight that validates the configured models are reachable before attempting summarization.

**Fixed in v0.2.3.**

---

## 5. `indexa --version` reports wrong version (same as issue 1, but broader: Cargo.toml version never bumped)

Already covered in issue 1.

---

## 6. Config and data directories use inconsistent bundle IDs (minor)

**Observed:**  
- Index DB: `~/Library/Application Support/indexa/index.db`  
- Config file: `~/Library/Application Support/dev.indexa.indexa/config.toml`

The config path uses a reverse-DNS bundle-ID style (`dev.indexa.indexa`) while the data path uses a plain name (`indexa`). On macOS the `directories` crate routes `config_local_dir()` through a different macOS API than `data_local_dir()`, which picks up the bundle identifier from the running process's `Info.plist` if present. Since the distributed binary has no `Info.plist`, the paths differ based on OS defaults.

**Impact:** Low — both paths work, but a user looking for their config file may not find it in the same parent directory as their index database.

**Expected fix shape:** Explicitly construct both paths from `BaseDirs::home_dir()` with a consistent `~/.indexa/` prefix, bypassing the OS directory resolution, or document the two paths clearly in `indexa status`.

**Fixed in v0.2.3.**
