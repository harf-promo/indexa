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

## 4. Ask it something

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
