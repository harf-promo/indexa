# Invariant audit & small-fixes review — 2026-08-30

Lane 6 of the six-lane parallel round (see `.orchestration/lanes.yml`). Scope: re-verify
every AGENTS.md "load-bearing invariant" / "verified non-bug" claim against the code as it
stands today, and run a general correctness sweep of areas not owned by the other five
lanes this round (`crates/parsers/**`, `crates/llm/**`, untouched CLI commands, misc
`crates/core` files). No new features implemented.

## Part 1 — Invariant verification

All seven AGENTS.md-listed invariants in this lane's scope were checked against the code
and tests, and **all seven still hold**. No invariant-related fix was needed.

| # | Invariant | Verdict | Evidence |
|---|---|---|---|
| 1 | `apply_archive_penalty` (×0.15) / `apply_code_intent_boost` (×1.6) applied inside `retrieve()`, tests pass | ✅ Holds | `crates/query/src/qa/retrieve.rs:53` and `:64` call both inside `retrieve()`. `DEFAULT_ARCHIVE_PENALTY = 0.15` (`crates/core/src/config.rs:489`), `CODE_INTENT_BOOST = 1.6` (`crates/query/src/qa/retrieve.rs:419`). `cargo test -p indexa-query qa::tests` → 76 passed, 0 failed (1 ignored live-Ollama A/B test, expected). |
| 2 | `cargo tree -i openssl-sys` empty for default and `aarch64-unknown-linux-gnu` targets | ✅ Holds | Both invocations return "package ID specification `openssl-sys` did not match any packages" — i.e. genuinely absent from the dependency graph, not merely unbuilt. |
| 3 | Web fragment self-check test present, asserts every on-disk JS/CSS file under `assets/ui/` is in the concat list | ✅ Holds | `crates/web/src/lib.rs:637` `every_ui_fragment_on_disk_is_wired_into_the_concat_list` walks `assets/ui/{js,css}` via `std::fs::read_dir` and asserts each filename appears in `lib.rs`'s own source as `include_str!("../assets/ui/<sub>/<name>")`. `cargo test -p indexa-web every_ui_fragment_on_disk_is_wired_into_the_concat_list` → passes. |
| 4 | `resource::compute_budget` keys on `available_bytes`, not `total − used_memory()`; no `micro_benchmark` dead field | ✅ Holds | `crates/core/src/resource.rs:543-552`: primary path is `sample.available_bytes`; `total_ram_bytes.saturating_sub(used_bytes)` is reached only as a legacy fallback when `available_bytes == 0`, exactly as documented in the preceding comment block. `grep -rn micro_benchmark` across the whole workspace: zero hits. |
| 5 | `update_control.rs::wait_for_command` copies the value out before `send(None)`, no self-deadlock | ✅ Holds | `crates/web/src/update_control.rs:41`: `let current = *rx.borrow_and_update();` — the watch `Ref` guard is a statement-scoped temporary, dropped at the `;` before `CHANNEL.0.send(None)` on the next line runs. `cargo test -p indexa-web wait_resolves_to_sent_command_then_resets` → passes. |
| 6 | Fingerprint matcher stays hand-rolled `*`/`?` glob, correctly rejects `**` | ✅ Holds | `crates/core/src/fingerprint.rs:144` `glob_match` is hand-written (char-by-char backtracking), explicitly rejects any pattern containing `**` (matches nothing) rather than degenerating to single-`*` semantics. `crates/core/Cargo.toml` does not depend on `globset` at all (only `crates/parsers/src/preprocess.rs` uses it, for an unrelated purpose). `cargo test -p indexa-core glob_matcher_handles_star_and_question` → passes. |
| 7 | `directory_apps` covered by `orphan_rows_for`/`seed_full_entry`; app-detection is a sibling of `run_detectors`, not folded in | ✅ Holds | `crates/core/src/store/tests/mod.rs:149` (`orphan_rows_for`) and `:196-210` (`seed_full_entry` → `replace_apps_for_dir`) both cover the table. `apps/indexa/src/commands/index.rs:72-90` `detector_pass()` calls `detectors::run_detectors(...)` then, in the same function but as a separate, independently-error-handled step, `app_detect::detect_directory_apps(...)` — the doc comment above `detector_pass` states this explicitly ("Also runs the application/structure recognition pass (v0.66) as a sibling"). |

No AGENTS.md-documented invariant needed correction. Recommend AGENTS.md's invariant list
be left as-is.

## Part 2 — General correctness sweep

Scope: `crates/parsers/**`, `crates/llm/**`, and CLI commands / misc `crates/core` files
not owned by another lane this round (excluding `crates/query/**`, `crates/mcp/src/packs.rs`,
`crates/web/assets/ui/**`, `crates/embed/src/batcher.rs`, `crates/core/src/decisions/**`,
`crates/core/src/summary_drift.rs`, `crates/core/src/resource.rs`, `crates/core/src/fingerprint.rs`,
`crates/core/src/store/**`, and `apps/indexa/src/commands/deep.rs`).

Three parallel `review`-agent passes covered `crates/llm/`, `crates/parsers/`, and the
remaining CLI commands / misc `crates/core` files respectively. Eight real findings came
back: three in `crates/llm/`, two in `crates/parsers/`, and three in the CLI commands.
Seven fixes shipped directly in this PR (see below); one of the `crates/parsers/`
findings — a single-member-decoder gap affecting both the gzip and zstd codecs in
`compressed.rs` — is only half-fixed: the gzip half has a drop-in library fix and shipped,
but the zstd half needs new decode-loop logic and is written up below instead. Combined
with the fully-unfixed `update.rs` finding, that leaves two write-ups needing a deliberate
follow-up decision:

### Not fixed — needs a deliberate follow-up decision

**`crates/parsers/src/compressed.rs` — zstd codec only decodes the first frame of a
multi-frame `.zst` file (Medium, not fixed).** Same root cause as the gzip finding fixed
below, but `ruzstd::decoding::StreamingDecoder`'s own doc comment states the caveat
explicitly: *"expects the underlying stream to only contain a single frame... To decode
all the frames... the calling code needs to recreate the instance of the decoder and
handle `SkipFrame` errors by skipping forward"* ([ruzstd upstream issue
#57](https://github.com/KillingSpark/zstd-rs/issues/57)). Unlike gzip (where `flate2`
ships a drop-in `MultiGzDecoder`), there is no drop-in multi-frame reader here — a correct
fix means writing a decode-loop around `StreamingDecoder` that recreates it per frame,
handles `SkipFrame`, and re-derives the `MAX_ZIP_ENTRY_BYTES` bomb-guard cap *across*
frames (the current `.take(cap + 1)` is single-decoder-instance-scoped) without risking an
infinite loop on a malformed/looping stream. That's real, careful new logic, not a
same-day swap — recommend a dedicated follow-up PR. Concrete impact meanwhile: a
concatenated multi-frame `.zst` (e.g. produced by `zstd --long` multi-frame output, or
`cat a.zst b.zst`) indexes only its first frame, silently. (Also noted in passing: the
`.tar.gz` listing path in `crates/parsers/src/archive.rs:145` uses the same single-member
`GzDecoder`, but concatenated tar streams don't actually parse as more entries after the
first archive's end-of-archive markers even with a multi-member gzip reader underneath, so
there's no equivalent fix to make there — left as-is.)

**`apps/indexa/src/commands/update.rs::cmd_update` — non-interactive invocation without
`--yes` silently self-updates with no confirmation (Low/Medium, not fixed).** The
confirmation prompt is gated on `stdin.is_terminal()`; when stdin is redirected/piped
(cron, CI, a wrapper script) AND `--yes` wasn't passed, the `if` block is simply skipped —
no prompt, no bail — and the binary is downloaded and replaced. This reads as a real gap
against the command's own documented contract (`crates/cli/src/lib.rs:939`: `"indexa
update # check, confirm, then update"`, and `-y`/`--yes`'s help text implies confirmation
otherwise always happens), and it's the single highest-consequence instance of this shape
in the CLI (self-replacing the running binary). However, the reviewing agent also found
that **the same "non-interactive silently proceeds" shape is already used elsewhere in
this codebase** — `cmd_weight_apply` (`weight.rs:157-170`) and `pack.rs`'s `--auto` create
path (which carries an explicit `// non-interactive: accept` comment) — so this may be a
deliberate house convention rather than a one-off oversight, and tightening `update.rs`
alone (to `bail!` without `--yes` in a non-interactive session, matching
`helpers.rs::check_huge_root_guard`'s pattern) is a **behavior change** for any existing
non-interactive `indexa update` callers, not a pure bug fix. Recommend a deliberate
decision (and, if made, a matching audit of `weight.rs`/`pack.rs` for consistency) rather
than a silent same-day patch to the self-update path specifically.

### Fixed directly (see "Direct fixes shipped in this PR" below)

1. `crates/llm/src/ollama.rs::ollama_pull` — reported success on a stream that closed
   before the terminal `"success"` frame.
2. `crates/llm/src/claude_code.rs::claude_status` — both CLI probes leaked their child
   process on timeout (missing `.kill_on_drop(true)`).
3. `crates/llm/src/openai_compat.rs::ChatMessageResp` — hard-failed deserialization on a
   `"content": null` response shape some OpenAI-compatible backends send.
4. `crates/parsers/src/compressed.rs` — the gzip codec only decoded the first member of a
   multi-member `.gz` file (the documented `.log.gz` rotation use case).
5. `crates/parsers/src/{html,svg,org,code}.rs` + `office.rs`'s CSV path — hard-erroring on
   non-UTF-8 input instead of the lossy fallback `TextParser`/`MarkdownParser` already use.
6. `apps/indexa/src/commands/multimodal.rs` — vision-model readiness resolved against the
   embedder's Ollama host instead of the describer's.
7. `apps/indexa/src/commands/weight.rs::cmd_weight_delete` — an explicit `--kind file`
   also silently deleted the unrelated `dir`-kind weight for the same target.

## Direct fixes shipped in this PR

Each fix below ships with its own regression test (added in the same file/commit) and was
self-critiqued before opening the PR.

| # | File | Bug | Fix | Regression test |
|---|---|---|---|---|
| 1 | `crates/llm/src/ollama.rs` | `ollama_pull` returned `Ok(())` as soon as the byte stream ended, without checking that a terminal `"status":"success"` NDJSON frame was ever seen — so a connection that drops mid-pull (server restart, proxy truncation) was reported as a completed pull. The only caller (`offer_to_pull` in `helpers.rs`) trusts that `Ok`, so a user could be told a model is ready when it isn't. | Track `saw_success` across the stream; `bail!` if the stream ends without it — mirroring the sibling `generate_stream`/`stream_with_model` methods in the same file, which already bail on a missing `done: true`. Extracted the NDJSON-line-consuming loop into a pure `consume_pull_buffer` helper so this is unit-testable without a live/mocked HTTP server. | `pull_buffer_without_final_success_frame_leaves_saw_success_false`, `pull_buffer_with_final_success_frame_sets_saw_success_true`, `pull_buffer_leaves_trailing_partial_line_for_next_chunk`, `pull_buffer_error_frame_bails` |
| 2 | `crates/llm/src/claude_code.rs` | `claude_status`'s two probe subprocesses (`claude --version`, `claude auth status --json`) lacked `.kill_on_drop(true)`, unlike `run()` (which sets it explicitly "so a hung `claude` can't leak as a zombie"). When the 5s probe timeout fires and `tokio::time::timeout` drops the future, the child process kept running detached instead of being killed. This runs on every Settings-page load and every `doctor` run. | Add `.kill_on_drop(true)` to both `Command` builders, matching `run()`. | `timed_out_version_probe_actually_kills_the_child_process` (Linux-only, `#[cfg(target_os = "linux")]`, mirroring the existing `#[cfg(unix)]`-gated platform tests elsewhere in the codebase): spawns a fake `claude` script that sleeps 30s and records its own pid, calls `claude_status` against it, and asserts via `/proc/<pid>` that the process is gone after the 5s timeout — this test fails without the fix. |
| 3 | `crates/llm/src/openai_compat.rs` | `ChatMessageResp.content: String` (not `Option<String>`) hard-fails `.json()` deserialization when a backend sends `"content": null` — a real shape from Azure-style content-filtered/tool-call-only completions, which this module's own doc comment says it targets (llama.cpp, LM Studio, etc.). | `content: Option<String>`, `.unwrap_or_default()` at the call site — same pattern already used for `anthropic.rs`'s equivalent `ContentBlock.text` field. | `chat_response_with_null_content_deserializes_to_empty_string`, `chat_response_with_string_content_deserializes_normally` |
| 4 | `crates/parsers/src/compressed.rs` | The gzip codec used single-member `GzDecoder`, which stops cleanly (no error) after the FIRST gzip member of a concatenated multi-member file — exactly what log-rotation tooling produces (`cat a.gz b.gz > combined.gz`), and this module's own doc comment names rotated `.log.gz` as a primary target. Silent data loss: only the first segment ever got indexed. | Swap to `flate2::read::MultiGzDecoder`, a drop-in `Read`-interface replacement that decodes every member; the existing `.take(cap + 1)` bomb-guard cap is unaffected. | `parse_decodes_every_member_of_a_multi_member_gzip_file`: a 2-member concatenated `.gz` fixture, asserts both members' content is indexed (fails on the old `GzDecoder`). |
| 5 | `crates/parsers/src/{html,svg,org,code}.rs`, `office.rs` (`parse_csv`) | Five parsers called strict `std::fs::read_to_string(path)?` and propagated the error, unlike `TextParser`/`MarkdownParser` (which use `read_text_lossy` — BOM-sniff + lossy-UTF-8 fallback, config-driven via `[parsers] encoding`, default `Auto`) and unlike the adjacent docx/rtf/ppt branches in `office.rs`'s own dispatch (which degrade to a stub rather than hard-erroring). Concrete failure: a Windows-1252/Latin-1 CSV export from Excel, a legacy-encoded HTML/SVG/`.org` file, or a source file with one stray non-UTF-8 byte in a comment/string literal hard-fails the whole file under the (default) `Auto` encoding config instead of indexing what can be recovered. | `html.rs`/`svg.rs`/`org.rs`/`office.rs`'s `parse_csv` now call `read_text_lossy(path, chunk.encoding)`, honoring the same `[parsers] encoding` config `TextParser` does. `code.rs`'s `CodeParser` doesn't participate in per-call chunk-param threading at all (by design — see `Parser::parse_chunked`'s doc comment), so it uses the pure `decode_text_bytes` helper directly (always lossy), matching the config's own `Auto` default without adding threading that contradicts that design. | One new non-UTF-8-input test per file: `html_with_invalid_utf8_bytes_still_parses_under_the_default_encoding`, `svg_with_invalid_utf8_bytes_still_parses_under_the_default_encoding`, `org_parser_with_invalid_utf8_bytes_still_parses_under_the_default_encoding`, `parses_source_file_with_invalid_utf8_bytes` (code.rs), `csv_with_invalid_utf8_bytes_still_parses_under_the_default_encoding` (office.rs) — each fails without its corresponding fix. |
| 6 | `apps/indexa/src/commands/multimodal.rs` | `multimodal_readiness` resolved the vision-model check against `cfg.embedding.base_url`, but captioning actually runs against `cfg.describer.base_url` (see `deep.rs`'s captioner setup) — the two are independently configurable (per the existing `ollama_requirements_resolves_each_provider_against_its_own_base` test in `helpers.rs`). A user with the two pointed at different Ollama hosts got a readiness report checking the wrong host. | Resolve against `cfg.describer.base_url` instead; extracted the resolution into a small pure `vision_probe_base_url` helper for testability. | `vision_probe_resolves_against_describer_base_not_embedder_base` |
| 7 | `apps/indexa/src/commands/weight.rs::cmd_weight_delete` | The kind-matching loop (`if kind == k \|\| kind == "file" && k == "dir"`) widened ANY `kind == "file"` to also delete the `dir`-kind weight — including when the user passed `--kind file` **explicitly**. `indexa weight delete <path> --kind file` after `indexa weight set <path> dir 2.0` silently deleted the unrelated dir-kind weight too. | Split into a pure `kinds_to_delete(resolved_kind, explicit)` helper: the file→also-try-dir widening now applies only when `--kind` was NOT given (genuine auto-detection ambiguity), never when the user specified it explicitly. | `explicit_file_kind_does_not_also_delete_dir` (fails without the fix), plus `auto_detected_file_kind_also_tries_dir`, `dir_kind_never_widens_to_file_explicit_or_not`, `category_kind_is_exact_explicit_or_not` covering the preserved/unaffected cases. |

No dependency was added, removed, or version-bumped by any of these fixes (the `MultiGzDecoder`
swap uses an existing `flate2` type; everything else reuses existing crate-internal helpers) —
`apps/indexa-desktop`'s pinned lockfile needed no regeneration.

Full local verification run for this PR: `cargo fmt --check` ✅, `cargo clippy --workspace -- -D
warnings` ✅ (zero warnings), `cargo test --workspace` ✅ (every crate's suite green, only
pre-existing `#[ignore]`d live-network/live-tool tests skipped), `cargo build --release` ✅.
