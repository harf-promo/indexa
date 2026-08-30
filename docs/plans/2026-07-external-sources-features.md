# Feature Plan — Learnings from 5 External Sources

> Status: **shipped in full** — every item in this backlog (Top 8 + Waves 1–4, 24 features) landed in
> the "4-wave feature backlog from external sources" entry of [CHANGELOG.md](../../CHANGELOG.md)
> under `[0.77.0]`; the MCP tool surface it grew (47→50) later reached 51 via unrelated follow-up
> work. This document is now a historical design record of the plan, not a live backlog — CHANGELOG.md
> is canonical for what shipped and how; per-item "✅ shipped" notes below point back to it rather
> than re-narrating the detail. Written 2026-07-19.
>
> Sources studied: the Google Cloud [Open Knowledge Format blog post](https://cloud.google.com/blog/products/data-analytics/how-the-open-knowledge-format-can-improve-data-sharing),
> [GoogleCloudPlatform/knowledge-catalog](https://github.com/GoogleCloudPlatform/knowledge-catalog),
> [ripgrep](https://github.com/burntsushi/ripgrep),
> [GitNexus](https://github.com/abhigyanpatwari/GitNexus), and
> [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp).

Indexa is in consolidation (post-v0.76): most COMPETITIVE.md gaps are closed, so these sources are
used for targeted deepening of existing moats (pack interoperability, code-graph intelligence,
index freshness/trust, agent distribution), not new directions.

Decisions taken while planning: **full backlog** scope (everything learned, prioritized, plus an
explicit rejected list), and **new MCP tools may be added** where they're the cleanest surface —
the pinned `doc_tool_count_matches_code` test and docs are updated once per release that changes
the count.

Standing invariants every feature respects: local-first permanent (no team/sync/cloud), XML/Markdown
exports never HTML, zero-frontend-library web UI, openssl-free tree, minimal new deps, default-off +
fail-open for behavior changes, retrieval changes eval-gated via `indexa eval`, local models only.

---

## What each source taught (condensed)

### 1. Open Knowledge Format (blog + knowledge-catalog repo)
- **OKF v0.1 spec** (`okf/SPEC.md`): a knowledge bundle = a plain directory of Markdown files with
  YAML frontmatter — one file per concept. Required field: `type` (free-form). Recommended:
  `title`, `description`, `resource` (URI to underlying asset), `tags`, `timestamp` (ISO 8601).
  Cross-links are ordinary markdown links; relationship semantics live in prose.
- **Progressive disclosure via `index.md`**: one line per child (`* [Title](url) - description`) so
  an agent reads a cheap index first — Indexa's token-savings pitch applied to the export artifact.
- **`log.md`**: per-bundle chronological changelog (**Creation**/**Update**/**Deprecation** entries).
- **Tolerant-consumer conformance**: never reject bundles for unknown fields/types/broken links;
  preserve unknown keys on round-trip; explicit `okf_version`.
- **mdcode round-trip pattern**: export → hand-edit → re-import with checksum conflict detection.
- **Discovery predicate grammar**: `field=value` / `field:value` parsed out of free-text queries.

### 2. ripgrep
Already borrowed: parallel walker + `WalkState::Skip`, GitignoreBuilder, NUL-sniff binary heuristic
(`[scan] skip_binary`). Still unborrowed:
- **`--pre` preprocessor hook** with glob gating — external command turns any format into text.
  Indexa can beat rg: an indexer parses once per file version, so preprocessor cost is amortized.
- **Layered ignore files** (`.gitignore < .ignore < .rgignore`, `!` re-include). Indexa has no
  `.indexaignore`; users can't tune indexing without touching git behavior.
- **Encoding**: BOM sniff + UTF-16→UTF-8 transcoding (`encoding_rs`). **Confirmed Indexa gap** —
  all text-family parsers are strict-UTF-8; UTF-16LE files silently fail to parse today (and their
  NULs make the binary sniff misclassify them).
- **Named file-type sets**, **`--search-zip`** single-stream decompression, **explicitly-named-file
  bypass** of binary filtering, config observability via `--debug`.

### 3. GitNexus
- **Rich typed schema**: 31 node / 27 edge types in one edge table + per-edge confidence/evidence.
  Indexa stores only bare `imports|defines|calls` rows — no symbol kinds, line ranges, or heritage.
- **Depth-grouped risk-labeled impact** (d=1 "WILL BREAK" / d=2 "LIKELY AFFECTED" / d=3 "MAY NEED
  TESTING") — Indexa's `blast_radius` returns a flat list.
- **`detect_changes`** (git diff → symbols → blast radius), **`context`** (360° symbol view),
  **`trace`** (shortest path A→B), **precomputed communities/processes** = one-call architecture map,
  **Mermaid generation on export**, machine-parseable `[[path:lines]]` citation grounding.

### 4. codebase-memory-mcp
- **Git-poll watcher**: adaptive interval (5s + 1s/500 files, cap 60s); baselines commit only on
  successful reindex — changes never silently lost.
- **Tool profiles** (Scout/Analysis/All): restricted subsets un-advertised AND un-callable — cuts
  the token cost of injecting 47 tool schemas per session.
- **Staleness/coverage attestation**: generation stamps, coverage checks, unparseable files as
  first-class "missed" records.
- **Lifecycle hooks installer**: SessionStart injects context + freshness; PreToolUse observes
  Grep/Glob and injects graph hits; fail-open, context-only.
- **Co-change edges** from `git log`; **ADRs as graph nodes linked to code**.
- Notably it has **no** entity-relation-observation memory model — its "memory" IS the graph.
  Indexa's add_note/acontext/ledger already exceed it on freeform memory; the real gaps are
  freshness, linking, and proactivity.

## Confirmed Indexa gaps these sources address

1. Exports drop provenance the store already has (`SummaryRecord.model/source_hash/generated_at`).
2. No pack import/round-trip, versioning/history, or pack changelog.
3. Text parsing is strict UTF-8 — UTF-16/legacy encodings silently skipped.
4. No runtime preprocessor hook (Plugin SDK is compile-time only).
5. No `.indexaignore`; no positive type-set filters.
6. Singly-compressed files (`.gz`) never content-indexed (archives are list-only).
7. Graph: no symbol kinds/spans/heritage; tiers computed but not surfaced uniformly; flat
   blast_radius output; no diff→impact; no path tracing; 360° view takes 3 round-trips.
8. No freshness automation by default; no staleness flags on results; no coverage attestation.
9. Notes/decisions not linked to graph symbols; note surface is add-only.
10. 47 tool schemas injected into every MCP session; no profiles.
11. No agent lifecycle-hook distribution story (was the one COMPETITIVE.md gap still open at the
    time this was written; closed by Wave 3.1's `install-hooks` + SKILL.md).

---

## Top 8 by value (product ranking)

All 8 rows below shipped (see the Wave sections and `Remaining owner decision points` below).

| # | Feature | Wave item | Moat | Size |
|---|---------|-----------|------|------|
| ★1 | `indexa install-hooks` + shipped SKILL.md | 3.1 | Distribution — the declared open gap | M |
| ★2 | Diff-aware impact `changed_impact` | 2.3 | Graph/retrieval; strongest cross-source signal (2 competitors built it) | M |
| ★3 | Risk-grouped `blast_radius` output | 1.1 | Trust/agent usability; best value-per-effort in the set | S |
| ★4 | OKF pack export + `index.md` + provenance | 4.2 + 1.4 | Pack interop + trust | S–L |
| ★5 | Staleness attestation on citations | 1.2 | Trust — no competitor's answers admit staleness | S |
| ★6 | UTF-16/BOM transcoding | 1.3 | Correctness bug wearing a feature costume | S–M |
| ★7 | Note/decision anchoring to graph nodes | 2.6 | Memory × graph intersection only Indexa can own | M |
| ★8 | MCP tool profiles | 3.2 | Distribution; the token pitch applied to Indexa itself | S–M |

---

## Wave 1 — Quick wins (S-class, no schema changes) — ✅ shipped

All 8 items below shipped in `[0.77.0]`'s external-sources-backlog entry.

### 1.1 ★ Risk-grouped, depth-labeled `blast_radius` output *(GitNexus)*
- **What:** group hits by hop with `d=1 WILL BREAK / d=2 LIKELY AFFECTED / d=3+ MAY NEED TESTING`
  plus a one-line risk summary (LOW/MEDIUM/HIGH by direct-caller count). Pure formatting over data
  already computed.
- **How:** `blast_radius_resolved` in `crates/core/src/store/edges.rs` (~765) already tracks
  direct-vs-transitive and tiers — thread the hop number into the result struct (in-memory only).
  Format in `crates/mcp/src/graph.rs` and `apps/indexa/src/commands/graph.rs` (`--blast`).
- **Surface:** additive MCP param `grouped: bool` (default false → flat output preserved); CLI
  `--grouped`. Consider flipping the default after a release of soak.
- **Risk:** none when unset. **Size: S.**

### 1.2 ★ Staleness attestation on `ask`/`search` citations *(codebase-memory-mcp)*
- **What:** cited files get `(stale: modified since indexed)` when disk mtime > `indexed_at`; answer
  footer gains "index last updated …; N of M cited files changed since". Retrieval finally admits
  when it serves old text — extends v0.24 "Always Current" to per-answer granularity.
- **How:** at answer assembly in `crates/query/src/qa/synthesize.rs` (+ retrieval-only/catalog
  paths): one `fs::metadata` per cited file vs `chunks.indexed_at`/`entries.modified_s` (reuse
  `chunks_current_for_mtime`, `crates/core/src/store/chunks.rs`). Surface in
  `crates/mcp/src/retrieval.rs` and web answer rendering.
- **Surface:** annotation-only, default on; `[retrieval] staleness_flags` kill switch.
- **Risk:** fail-open (stat error ⇒ no flag); test asserts annotations never affect ranking. **Size: S.**

### 1.3 ★ UTF-16/BOM detection + `encoding_rs` transcoding *(ripgrep)*
- **What:** BOM-sniff (FF FE / FE FF / EF BB BF), transcode to UTF-8 with U+FFFD replacement.
  Fixes silent data loss for PowerShell redirects, `.resx`, Windows logs/CSVs.
- **How:** three coordinated touch points:
  `crates/parsers/src/text.rs` — replace `read_to_string` with a `read_text_lossy` helper
  (`encoding_rs` is already in Cargo.lock transitively; promote to a direct dep of indexa-parsers —
  no new tree entry, openssl-free); reuse across html/org/svg/ipynb/CSV parsers in a follow-up.
  `crates/parsers/src/registry.rs` — `looks_like_text` checks UTF-16 BOM before NUL/UTF-8 checks.
  `crates/core/src/text.rs` — `is_binary` gets the same BOM exemption so `[scan] skip_binary`
  doesn't drop UTF-16 files.
- **Surface:** `[parsers] encoding = "auto"` (default) | `"utf-8"` (old behavior) | forced label
  (rg's `-E`). "auto" only changes outcomes for files that previously errored.
- **Risk:** corpus changes ⇒ run `indexa eval` (sparse gate + `dense-eval` workflow) before merge;
  decode failure falls back to the old skip path. **Size: S–M.**

### 1.4 ★ Export provenance — stop dropping summary metadata *(OKF)*
- **What:** emit `model`, `generated_at`, `source_hash` (all already in `SummaryRecord` but dropped
  by every renderer) plus an OKF-inspired `resource` (path + content hash) so consumers can detect
  staleness and provenance. Stamp a `pack_format_version` now, while the format is young.
- **How:** `crates/query/src/export.rs`: `render_xml` adds attributes on `<summary>`; `render_json`
  adds fields; `render_markdown` adds a provenance comment line (match the notes pattern in
  `crates/core/src/notes.rs`). Escaping via existing `xml_escape_attr`. Keep everything inside the
  single `export_pack_body` redaction path (`crates/mcp/src/packs.rs` ~340). Update golden tests.
- **Surface:** on by default (additive attrs; document OKF's tolerant-consumer posture as the contract).
- **Risk:** golden-test churn only. **Size: S.**

### 1.5 `.indexaignore` custom ignore file *(ripgrep)*
- **What:** highest-precedence per-directory ignore layer with `!` re-include — tune indexing
  without touching git behavior (e.g. `!docs/generated/`, exclude committed-but-noisy fixtures).
- **How:** `crates/core/src/walker.rs`: one `add_custom_ignore_filename(".indexaignore")` call on
  the `WalkBuilder` (verified: not called today). Parity: `build_scan_matchers`/`should_index_file`
  (watcher path) and `build_ignore_matcher` (non-git roots) must load the same file or scan and
  watch disagree. Test alongside `walk_is_thread_count_invariant_and_still_prunes`; verify
  `is_sensitive_dir` still trumps re-includes.
- **Surface:** file presence is the opt-in; optional `[scan] custom_ignore = true` kill switch.
- **Risk:** inherently fail-open (no file ⇒ byte-identical). **Size: S.**

### 1.6 Mermaid dependency diagram in pack export *(GitNexus)*
- **What:** a fenced ```mermaid block — pure text (never HTML), renders natively in most AI tools
  and viewers. `render_graph` exists in `export.rs` but isn't wired into the pack-export path.
- **How:** new `render_graph_mermaid` (flowchart syntax over the same `code_graph_scoped` data);
  wire `include_graph` through `export_pack_body`, CLI PackAction flags (`crates/cli/src/lib.rs`
  ~873; handler `apps/indexa/src/commands/pack.rs`), and `/api/packs/:name/export`
  (`crates/web/src/handlers/packs.rs`). Reuse the existing 200-edge cap.
- **Surface:** CLI `--include-graph[=mermaid|text]`; additive MCP `export_pack` param. Default off.
- **Risk:** none unset; size guarded by caps + `--token-budget`. **Size: S–M.**

### 1.7 `indexa doctor` config observability *(ripgrep `--debug`)*
- **What:** doctor prints which config file was loaded and every non-default key in effect (later:
  active preprocessors/type sets).
- **How:** `crates/core/src/config.rs` gains `non_default_keys()` diffed against
  `Config::default()`; print in the doctor handler. Exclude API keys (never logged, per AGENTS.md).
- **Size: S.**

### 1.8 Predicate grammar in search queries *(knowledge-catalog SKILL)*
- **What:** `ext:md`, `path:crates/core`, `type=code`, `pack:auth`, `lang:rust` parsed out of the
  free-text query of MCP `search`/`ask`/`search_pack` — structured filtering, no new params.
- **How:** new `crates/query/src/predicates.rs` pure parser: extract `known_field(:|=)value` tokens
  (known-field allowlist only — `"see path: below"` is never eaten), map to existing scope/category/
  extension filters inside `retrieve()` (`crates/query/src/qa.rs`); wire in
  `crates/mcp/src/retrieval.rs` before retrieval. Document the grammar in tool descriptions.
- **Surface:** `[retrieval] query_predicates = false` — default OFF first release, flip after eval + soak.
- **Risk:** eval-gated; unparseable predicate ⇒ literal text (fail-open). **Size: M.**

---

## Wave 2 — Graph & memory deepening (schema work; spine is 2.1 → 2.2 → 2.3) — ✅ shipped

All 7 items below shipped in `[0.77.0]`'s external-sources-backlog entry.

### 2.1 Symbols table: kind + line ranges *(GitNexus/codebase-memory; prerequisite for 2.3/2.5)*
- **What:** today `defines` rows are bare names — no kind, no positions. A `symbols` table unlocks
  diff mapping, kind-aware output ("`validate` (function, src/auth.rs:45-60)"), disambiguation, and
  future line-anchored citations.
- **How:** `crates/core/src/store/schema.rs` (additive `CREATE TABLE IF NOT EXISTS`; the copy-table
  migration precedent is the `edges_new` block at ~498):
  `symbols(path, name, kind, start_line, end_line, PRIMARY KEY(path,name,kind,start_line))`.
  Extraction: `crates/parsers/src/code.rs` `extract_defines` already walks named tree-sitter nodes —
  capture `node.kind()` (mapped to fn/struct/enum/trait/class/interface/method/const/type) and
  `start_position().row`/`end_position().row`. Persist via new `Store::upsert_symbols` (same
  delete-by-path-then-insert contract as `upsert_edges`) from `crates/web/src/jobs_exec/deep.rs`
  (~668) and `apps/indexa/src/commands/deep.rs`.
- **Surface:** none — populated on next `indexa deep`; readers fail open on an empty table.
- **Risk:** small write-volume bump on deep; no retrieval impact. **Size: M.**

### 2.2 Heritage edges (`extends`/`implements`) + uniform tier surfacing *(GitNexus)*
- **What:** class hierarchies / trait impls become visible; `blast_radius` can flag implementors of
  a changed trait. Also surface the existing `ResolutionTier` as coarse confidence uniformly across
  `dependencies` and `blast_radius` hops (`who_calls` already partially does).
- **How:** the `edges` CHECK constraint (`schema.rs` ~214) needs the proven `edges_new` copy-table
  migration to widen to `('imports','defines','calls','extends','implements')`. Extraction per
  language in `crates/parsers/src/code.rs` (Rust `impl_item` w/ trait; TS/Java `extends_clause`/
  `implements_clause`; Python class bases; C++ `base_class_clause`; Go is structural — skip,
  document). Traversal in `edges.rs`: heritage target→source as an extra caller-direction hop
  behind a param. Tool descriptions updated (doc-drift CI forces this).
- **Surface:** additive `include_heritage: bool` on `blast_radius`/`dependencies` (default false);
  CLI `indexa graph --heritage`.
- **Risk:** crash-safe migration via the proven pattern; default-off traversal keeps outputs
  byte-identical. **Size: M–L.**

### 2.3 ★ `changed_impact` — NEW MCP TOOL *(GitNexus `detect_changes` + codebase-memory-mcp; the
strongest convergence signal — two independent competitors built the identical tool)*
- **What:** "what did I just touch and what does it break" in one call: git diff → changed spans →
  symbols (via 2.1) → existing `blast_radius_resolved` per symbol → 1.1's grouped risk output.
  Indexa has zero git integration today (verified: no git2, no shell-outs in the Rust tree).
- **How:** new `crates/core/src/gitdiff.rs`: shell out `git -C <root> diff --unified=0
  [--staged | <base_ref>]`, parse `@@ -a,b +c,d @@` hunk headers — no git2 dependency; absent git /
  non-repo ⇒ clean error, fail open. Intersect changed ranges with `symbols` rows; files without
  symbols fall back to file-level `who_imports`. Dedicated tool with params
  `scope: "unstaged"|"staged"|"<base_ref>"`, `depth`, `strict`. CLI `indexa graph --blast --diff [ref]`.
  Update `doc_tool_count_matches_code` + docs (47→48 within this release's bump).
- **Risk:** read-only git invocation, 2s timeout on the child; degrades gracefully to file level
  without 2.1. **Size: M.**

### 2.4 `trace_path` — NEW MCP TOOL *(GitNexus `trace`)*
- **What:** BFS shortest path between two symbols/files ("how does handler A reach db fn B") —
  a question agents currently burn many `dependencies` calls approximating.
- **How:** `Store::trace_path(from, to, max_depth)` in `edges.rs`, reusing `calls_of` +
  `resolve_call` (same machinery as `dependency_closure` ~1000); ordered hops with file + tier per
  hop. Depth cap 10 for trace, bounded by `TRANSITIVE_CANDIDATE_CAP`. CLI `indexa graph --trace A B`.
- **Risk:** bounded traversal; count 48→49. **Size: S–M.**

### 2.5 `symbol_context` — NEW MCP TOOL *(GitNexus `context`)*
- **What:** 360° symbol view — incoming AND outgoing references categorized (calls, imports,
  heritage, definitions with kinds/spans from 2.1), plus anchored notes/decisions (2.6) — replacing
  3 round-trips (`who_calls` + `who_imports` + `dependencies`) with one. Ranked candidate
  disambiguation when the name is ambiguous (honoring the `symbol_ambiguity` ledger pin).
- **How:** aggregation over existing `edges.rs` lookups (`who_calls_resolved`, `edges_to`,
  `edges_from`, `definers_with_pin`); one new assembly fn + tool in `crates/mcp/src/graph.rs`.
  Count 49→50.
- **Risk:** read-only composition of existing queries. **Size: M.**

### 2.6 ★ Note/decision anchoring to code *(codebase-memory ADR-linking)*
- **What:** optional anchor (path or symbol) on `add_note`; graph tools then append
  "Note: <title> (<pack>)" when an anchored note/decision exists for what they're reporting —
  "your index remembers your judgment" surfaces exactly where agents look.
- **How:** additive `anchor` param on `add_note` (`crates/mcp/src/admin.rs` ~251); persist in the
  note's provenance comment (`crates/core/src/notes.rs` header gains `anchor=…`) AND a small
  `note_anchors(note_path, anchor, anchor_kind CHECK IN ('path','symbol'))` table for cheap joins.
  Join into `graph.rs` tool output + `export_pack_body`. Decisions already have `decision_paths` —
  surface in the same join for free.
- **Risk:** none by default; empty table ⇒ no output change. Tool count unchanged. **Size: M.**

### 2.7 Co-change edges from git history *(codebase-memory `FILE_CHANGES_WITH`)*
- **What:** files that historically change together — behavioral coupling invisible to static
  analysis. Boosts `related_files` recall; new web overlay layer.
- **How:** offline pass (`indexa graph --compute-co-change`, optionally at end of `indexa index`):
  `git log --name-only --no-merges -n 2000`, count pairs (skip commits touching >50 files),
  persist top-N to `co_change(path_a, path_b, count, computed_at)`. Readers:
  `find_related_files_resolved` gains an optional component; web overlay follows the exact
  `pack_edges.rs` pattern (request-time, fail-open, cost-guarded).
- **Surface:** `[graph] co_change = false` default OFF; additive `include_co_change` on `related_files`.
- **Risk:** inert by default; any future retrieval-scoring use is eval-gated. **Size: M.**

---

## Wave 3 — Distribution & freshness (independent of Wave 2; can run in parallel) — ✅ shipped

All 3 items below shipped in `[0.77.0]`'s external-sources-backlog entry.

### 3.1 ★ `indexa install-hooks` + shipped SKILL.md *(codebase-memory hooks + knowledge-catalog SKILL)*
- **What:** the distribution play for what was then the one COMPETITIVE.md gap still open (now
  closed by this item, per that file). (a) A command that
  prints — and only with `--write` applies, after showing a diff — Claude Code hook config:
  SessionStart injects `indexa status --brief` + overview/freshness; optional PreToolUse-on-Grep
  injects top `ask --catalog` hits as additionalContext. Fail-open, context-only, never blocking.
  Claude Code only (not 43 clients). (b) A shipped SKILL.md teaching agents how to query Indexa
  well (which tools for which questions, catalog-first progressive disclosure, predicate grammar
  once 1.8 lands) — installable via the existing `mcp install` family.
- **How:** new `apps/indexa/src/commands/hooks.rs` emitting JSON hook blocks that call the existing
  CLI; SKILL.md as a static asset installed next to the MCP config.
- **Risk:** writes user config only with explicit `--write` + diff; hooks are context-only.
- **Size: M.**

### 3.2 ★ MCP tool profiles *(codebase-memory Scout/Analysis/All)*
- **What:** `indexa mcp --tool-profile core` serves ~10 tools (search, ask, dependencies,
  who_calls, blast_radius, list_packs, search_pack, export_pack, add_note, list_open_decisions);
  the rest un-advertised AND un-callable. Cuts per-session schema token cost; makes Indexa cheap
  for subagents.
- **How:** profile filter at `tool_router()` composition (`crates/mcp/src/lib.rs` ~228) + dispatch
  guard. The pinned test keeps the full-profile count as the contract and gains per-profile
  assertions — profiles are subsets, never new tools.
- **Surface:** `--tool-profile` flag + `[mcp] tool_profile`; default `full` (zero change unless opted in).
- **Size: S–M.**

### 3.3 Git-poll auto-freshness worker mode *(codebase-memory watcher)*
- **What:** `[scan] auto_reindex = "git-poll"`: poll git state (HEAD moved / tree dirty) per repo
  root with adaptive interval (5s + 1s/500 files, cap 60s); on change run the existing incremental
  scan→deep; **baseline advances only on successful reindex** (changes never silently lost).
- **How:** extend `run_auto_reindex` in `apps/indexa/src/commands/worker.rs` (the sole consumer of
  `auto_reindex`); read `.git/HEAD` + `git status --porcelain --untracked-files=no` (or `.git/index`
  mtime). Non-git roots fall back to interval mode.
- **Surface:** new accepted config value; default stays `"off"`; activation surface unchanged
  (`indexa worker --auto-reindex`).
- **Risk:** git errors ⇒ log + retry next poll (fail-open). **Size: M.**

---

## Wave 4 — Packs interop & pipeline breadth (chains: pack_events → 4.2 → 4.3) — ✅ shipped

All 6 items below shipped in `[0.77.0]`'s external-sources-backlog entry.

### 4.1 `pack_events` history *(prerequisite; enables log.md + versioning-lite)*
- **What:** append-only `pack_events(pack_id, event CHECK IN ('created','path_added','path_removed',
  'renamed','exported'), detail, at)` written from existing CRUD in `crates/core/src/store/packs.rs`.
  Gives packs an `updated_at` (max event time) and a changelog source.
- **Size: S** (own small PR).

### 4.2 ★ OKF-conformant pack bundle export *(OKF blog + knowledge-catalog)*
- **What:** `indexa pack export <name> --format okf --out <dir>` writes an OKF v0.1 bundle:
  one `.md` per pack item with frontmatter (`type`, `title`, `description` = L0 abstract,
  `resource` = path + content hash, `tags`, `timestamp` = summary `generated_at`), a
  progressive-disclosure `index.md` (one-line entries — the token-savings pitch made physical),
  and a `log.md` generated from 4.1. Pure Markdown; do NOT emit OKF's viz.html (never-HTML).
  Packs stop being an Indexa-only artifact — consumable by Obsidian, Knowledge Catalog, any
  OKF-aware agent.
- **How:** new `render_okf_bundle` in `crates/query/src/export.rs` returning
  `Vec<(relative_path, content)>`; CLI handler writes the directory; MCP `export_pack` gains an
  additive `format:"okf"` value returning the bundle concatenated with `--- file:` separators.
  Redaction: run `redact_secrets` over frontmatter values too, inside `export_pack_body`.
- **Size: M** (after 4.1).

### 4.3 Pack import / round-trip curation *(knowledge-catalog mdcode)*
- **What:** `indexa pack import <bundle> [--force]` reconstructs a pack from a 4.2 export, with
  checksum conflict detection (manifest carries per-item hashes; fail fast if the underlying
  indexed files changed since export). Turns packs into curated, hand-editable knowledge sets.
  Same-machine / machine-migration scope — NOT team sharing; import validates paths exist in the
  index and refuses paths outside indexed roots; no remote fetching.
- **How:** manifest = bundle-root `index.md` frontmatter + `manifest.json` (`pack_format_version`,
  name, description, items[{path, hash}]); import → `create_pack` + `add_pack_paths` (already
  idempotent). Adopt OKF tolerant-consumer semantics verbatim (never reject unknown fields,
  preserve them — matches the additive-API-evolution practice). CLI-only initially.
- **Size: M** (after 4.2).

### 4.4 Runtime preprocessor hooks *(ripgrep `--pre`, improved)* — **owner security decision**
- **What:** `[[parsers.preprocessor]] glob = "*.dwg", command = "dwg2text", timeout_s = 30,
  max_output_mb = 16` — external command's stdout indexed as text; covers the long tail without
  shipping parsers. Better than rg: the deep phase's mtime/hash skip means the command runs once
  per file version, not per query — no new cache table needed.
- **How:** new `crates/parsers/src/preprocess.rs` `PreprocessorParser` implementing the existing
  `Parser` trait (`accepts_path` via `globset` — allowed here; the hand-glob pin covers only the
  fingerprint matcher), spawning cmd with path as argv[1] + bytes on stdin; caps in the spirit of
  the zip-bomb caps in `types.rs`; registered via the existing `Registry::register` prepend; runs
  inside `parse_guarded` (panic isolation). rg's graceful-fallback contract: nonzero exit / empty
  stdout / timeout ⇒ fall through to native parsers, never blank the file.
- **Surface:** config-file only (0600) — never enabled via MCP/web. Doctor (1.7) lists active hooks.
- **Risk:** arbitrary-command execution is the user's explicit opt-in via their own config; parked
  as a standalone decision, not bundled. **Size: M.**

### 4.5 Transparent gzip indexing for singly-compressed text *(ripgrep `-z`)*
- **What:** index content of `README.md.gz`, rotated `.log.gz`, man pages. gzip only — `flate2` is
  already in Cargo.lock; zstd/xz/brotli are new deps, deferred until demanded.
- **How:** new `crates/parsers/src/compressed.rs` accepting `*.gz` (NOT `.tar.gz`/`.tgz` — those
  stay with `archive.rs`): stream-decompress with a hard cap (mirror `MAX_ZIP_ENTRY_BYTES`), strip
  `.gz`, route inner name through normal registry dispatch (`foo.log.gz` → `.log` → text parser,
  including 1.3's encoding handling). Register before `Archive`.
- **Surface:** `[parsers] compressed = false` default OFF.
- **Risk:** bomb-guarded; decompress error ⇒ metadata-only as today; eval-gated (corpus changes).
  **Size: M.**

### 4.6 Architecture map: persisted modules overview *(GitNexus communities/processes)*
- **What:** the "orientation in one call" primitive: a clustered map of named functional areas
  over the import/call graph. Louvain already exists (`crates/core/src/store/communities.rs`) but
  request-time, web-only, unpersisted.
- **How:** post-deep pass (behind config) runs existing Louvain over `code_graph_scoped` + directory
  priors; persists `graph_modules(module_id, label, cohesion, member_path)`; labels via gemma3:12b
  from member L0 abstracts (one short local call per cluster, cluster count capped à la GitNexus
  `max(20, min(300, n/10))`, cached keyed on edge-set hash). Surface: additive `view:"modules"` on
  the existing `code_graph` tool; web reads the persisted table (`handlers/graph.rs`,
  `29-graph-communities.js`); `indexa graph --modules`. Clusters seed `pack create --auto`.
- **Surface:** `[graph] modules = false` default OFF (spends local-LLM time at index).
- **Size: L.**

---

## Rejected / deferred (with reasons)

**Already exists in Indexa — do not rebuild:**
- NUL binary sniff (`[scan] skip_binary` + `text::is_binary` + `looks_like_text`). Only micro-delta
  worth taking: rg's explicitly-named-file bypass for `indexa index <file>` (fold into any walker touch).
- Louvain community detection (web overlay) — 4.6 is exposure/persistence, not new algorithm work.
- Catalog / progressive-disclosure retrieval (`ask catalog:true`) — OKF's contribution is applying
  it to the *export artifact* (4.2), not a new mode.
- Hash-gated incremental re-embedding (`chunk_content_hash` + `cached_embeddings_by_hash`).
- Agentic multi-step ask (shipped v0.20, opt-in) — GitNexus's LangChain ReAct loop adds nothing local.
- Token-savings telemetry (savings ledger + `explain_savings`) — closed per CHANGELOG.
- Call-resolution confidence (ResolutionTier, v0.25) — GitNexus's decimal evidence weights are
  precision theater on top; only *uniform surfacing* is planned (2.2).

**Invariant conflicts:**
- Team-shared index snapshot committed to the repo (`graph.db.zst`) — team/sync REJECTED; raw DB may
  contain unredacted content; machine migration is covered by pack round-trip (4.3).
- viz.html in exports — never-HTML. Web UI fills the role.
- Sigma.js/Graphology/Cytoscape — zero-frontend-lib; hand-rolled SVG force layout exists (`19-graph.js`).
- Cloud-LLM web-crawl enrichment — local models only; the transferable guardrails (page caps,
  domain allowlist) already exist on `pack add-url`.
- Graph DB swap (LadybugDB/KuzuDB) — SQLite-everything is deliberate.
- `rename` tool (GitNexus) — Indexa serves context; it does not edit code.
- 43-client hook installer breadth — Claude Code only; writing dozens of third-party configs is
  out of scope and risk-heavy.

**Low value/cost or premature:**
- PDG/taint/dataflow layer — statement-level dataflow ×8 languages, outside the mission.
- Process tracing (entry-point execution flows) — heavy precompute; modules (4.6) deliver ~70% of
  the orientation value at ~20% of the cost. Revisit only if 4.6 proves demand.
- Cypher-like `graph_query` tool — parser + injection-review surface; revisit only as a
  *consolidation* play if graph tools multiply.
- ~~Named file-type sets~~ — **RESOLVED, shipped** (2026-08-02) after its two blockers landed
  (`.indexaignore` in Wave 1, preprocessor hooks in Wave 4). Its actual scan-time reconcile/prune
  interaction risk was confirmed real during design (a positive allowlist during `scan` would
  delete every non-matching file's index rows via the ordinary "unseen this run ⇒ ghost"
  mechanism) — shipped instead as a **query-time predicate** (`type:python` etc. in
  `search`/`ask`, `crates/query/src/predicates.rs`), which never touches the walker or reconcile
  and reuses the same mechanism Wave 1.8's `ext:`/`path:` predicates already proved safe.
- Query fan-out (3 gemma3 variants + RRF) — adds local-model latency per query; wait for eval
  evidence it lifts hit_rate.
- `task_context`/goal hint param on retrieve — boost plumbing exists; wait for demand.
- Entity-relation-observation memory model — neither competitor has one; Indexa already exceeds
  both on freeform memory. The real gaps (freshness/linking/proactivity) are covered by 1.2, 2.6, 3.1.
- In-process candle embedder (drop the Ollama prerequisite) — validated by codebase-memory's
  embedded int8 nomic, but stays deferred; re-raise when the reranker's candle plumbing stabilizes.
- mmap/streaming-read heuristics — already the planned streaming-scan work, not a new item.
- Pack `log.md` standalone — needs 4.1 first; bundled there.

---

## Remaining owner decision points

1. ~~**Preprocessor hooks (4.4)**~~ — **RESOLVED, shipped.** Owner-approved in Wave 4 (#400);
   `crates/parsers/src/preprocess.rs`, config-file-only, no MCP/web surface.
2. ~~**zstd/xz/brotli deps**~~ — **RESOLVED, shipped** in the compression-codec-expansion follow-up
   PR after Wave 4: `.zst` (`ruzstd`), `.xz`/`.lzma` (`lzma-rs`), `.br` (`brotli`), all pure-Rust,
   no C toolchain. Same `[parsers] compressed` flag gates all four codecs.
3. **`--grouped` default flip** for `blast_radius` (1.1) after a release of soak — **still open.**
   No release has shipped since Wave 1 introduced this feature, so the stated condition genuinely
   hasn't been met yet; owner chose to keep the default off for now (2026-08-02).
4. ~~**Hooks `--write`**~~ — **RESOLVED, shipped** in Wave 3 (#399): `apps/indexa/src/commands/
   hooks.rs` — print-only default, `--write` applies (keeps a `.bak`), exactly the confirmed UX.
5. ~~**MCP count evolution**~~ — **RESOLVED.** Landed at 50 tools (`changed_impact`/`trace_path`/
   `symbol_context`), pinned-test and docs updated together in Wave 2 (#398).

## Verification (per wave, before "done")

- Standard gate every PR: `cargo fmt --check` · `cargo clippy --workspace -- -D warnings` ·
  `cargo test --workspace` · `cargo build --release`.
- **Eval-gated items** (corpus or query interpretation changes): 1.3, 1.8, 4.5 — run `indexa eval`
  over `fixtures/self-golden.json` (sparse CI gate) and the `dense-eval` manual workflow; baseline
  vs branch per `docs/methodology.md`.
- **Golden/pinned tests:** export renderers (1.4, 1.6, 4.2) update golden outputs;
  `doc_tool_count_matches_code` updated in the release that adds tools (2.3/2.4/2.5);
  doc-drift CI keeps tool descriptions honest (2.2).
- **Graph work:** oracle-style tests in `crates/core/src/store/edges.rs` tests + `tests/graph.rs`
  (the `dependency_closure` tests at ~401 are the template); walker changes tested alongside
  `walk_is_thread_count_invariant_and_still_prunes`.
- **UI-touching items** (overlay for 2.7, modules view 4.6): `indexa serve` → visually confirm at
  http://localhost:7620; new JS files MUST be added to the `include_str!` concat list in
  `crates/web/src/lib.rs`.
- **MCP surface changes:** restart the MCP server after CLI update (CLI-skew invariant); verify
  via `indexa doctor` + a live tool call.
