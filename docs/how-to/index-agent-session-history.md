# Index your own Claude Code session history

**Goal:** search your *past AI conversations* the same way you search your code — "did I already
investigate this?" instead of re-explaining the same bug to a fresh session.

This is **opt-in only**. Indexa never auto-discovers or scans `~/.claude/projects/` on its own —
it parses a Claude Code session transcript only when you explicitly point a scan root at it, the
same as any other folder you choose to index.

## 1. Point Indexa at your session logs

Claude Code writes one JSON-lines transcript per session under
`~/.claude/projects/<project-slug>/<session-id>.jsonl`. Index a project's session history:

```bash
indexa deep ~/.claude/projects/-path-to-your-project
```

Or a specific session file:

```bash
indexa deep ~/.claude/projects/-path-to-your-project/2b7c1a9e-....jsonl
```

Indexa recognizes the transcript format by sniffing file **content** (each line's `type` and
`message` shape), not the `.jsonl` extension — an unrelated JSONL data file elsewhere on disk is
left alone and indexed as plain data, same as before.

## 2. What gets extracted

Only the human-readable conversation: your prompts, and the assistant's prose replies. Each
extracted turn becomes one or more searchable chunks, headed `Turn N [user]` / `Turn N [assistant]`
so results show who said it.

Everything else in the transcript is skipped:

- Tool-call and tool-result payloads (`tool_use`/`tool_result` blocks) — not prose, and often
  large JSON you don't want cluttering search results.
- The assistant's internal `thinking` blocks.
- Session-metadata lines Claude Code interleaves with the conversation (permission-mode changes,
  file-history snapshots, queue bookkeeping, …) — these carry no conversational content.

## 3. Secrets are redacted like everywhere else

Old conversations can contain things you pasted in and later regret — an API key, a token, a
connection string. Indexa's existing secret redaction (`redact_secrets`, already applied to
*everything* indexed — code, docs, email, now session transcripts too) covers this: a detected
secret is replaced with a `[REDACTED-<kind>]` marker before it's stored or exported. This is the
same redaction path every other file goes through; nothing transcript-specific was added or
weakened.

## 4. Scope `search` to transcript content only

Once indexed, `indexa deep` re-checks every `.jsonl`/`.ndjson` entry against the same
content-sniff `accepts_path` uses, and stamps a real transcript's index row with a
`category: agent-session` tag (fail-open — a `deep` run still succeeds even if this post-pass
has an issue). The MCP `search` tool can then scope to exactly that content — pass a `category`
param, or (with `[retrieval] query_predicates = true`, see `docs/config.md`) type a
`category:agent-session` token right into the query — and nothing else matches:

```json
{"query": "flaky retry test", "category": "agent-session"}
```

```
category:agent-session flaky retry test
```

This is more precise than the alternatives:

- `path:`/`scope` only works if your transcripts live under their own directory — no good if
  `~/.claude/projects/` is indexed alongside a project's own code and docs.
- `ext:jsonl` matches **any** `.jsonl` file, transcript or not — a JSONL data export, log file,
  or fixture sitting right next to a real transcript would also match.
- `category:agent-session` matches only entries the content-sniff itself confirmed are a real
  Claude Code transcript, regardless of where they live or what else shares the `.jsonl`
  extension.

`category:` is a `search`-only filter, like `ext:`/`type:` — on `ask`, a `category:` token is
still stripped out of the question text (so it doesn't pollute retrieval as a literal keyword)
but has no filtering effect there. Scope an `ask` to transcript content with a `search` call
first, or ask a question specific enough that transcripts are what's relevant.

## 5. Ask it something

```bash
indexa ask "did I already investigate the flaky retry test?"
```

If a past session covered it, the answer cites that conversation directly — the turn(s) where you
raised it and the assistant's findings — instead of you re-deriving the investigation from
scratch. Add `--explain` to see exactly which transcript chunks were retrieved:

```bash
indexa ask --explain "did I already investigate the flaky retry test?"
```

`synthesize:false` (via the MCP `ask` tool, or `--no-synthesize` on the CLI) returns the raw
retrieved slice instead of a synthesized answer, if you'd rather read the original turns yourself.

## Tips

- Re-indexing is incremental (`indexa deep` skips unchanged files by mtime), so re-running after a
  new session just adds the new transcript — it doesn't reprocess history you've already indexed.
- Combine with a [Context Pack](index-and-export-for-claude.md#context-packs--a-subject-not-a-folder)
  to group session history for one investigation alongside the code it touched.
- A transcript can be large (long sessions, verbose tool output) — Indexa streams it line by line
  rather than loading the whole file into memory, so this is safe on multi-GB logs.
