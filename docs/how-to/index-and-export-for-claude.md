# Index a repo and export it for Claude / Cursor

**Goal:** turn a codebase (or any folder) into one portable context file you can paste into — or
attach to — an AI tool, so it answers with grounding instead of guessing. This is the "free context
for paid AI tools" workflow: build the slice locally, spend zero cloud tokens on indexing.

## 1. Build the context

```bash
indexa index ~/code/myproject     # scan → deep embed → summaries, one command
```

Check it landed:

```bash
indexa status            # entries / chunks / embedded counts
indexa doctor            # if anything failed, this tells you why
```

## 2. Export it

```bash
# XML is the recommended format for AI tools (Anthropic's docs prefer XML structure)
indexa export ~/code/myproject --format xml > myproject.context.xml

# Markdown if you'd rather read it yourself; JSON for tooling
indexa export ~/code/myproject --format md   > myproject.context.md
```

Control the size with `--depth` (how far down the summary tree to expand) — a shallow export is a
high-level map; a deep export includes more file-level detail:

```bash
indexa export ~/code/myproject --format xml --depth 2 > overview.xml
```

### Slice to what matters

Instead of the whole tree, export a relational slice — the point of a context engine is to hand
the AI tool the part that's relevant, not the repo:

```bash
# Only what changed recently (windows: 7d, 12h, 90m, 3600s) — great for a "what's new" review
indexa export ~/code/myproject --changed-since 7d > recent.xml

# Only files in a classification category (code / document / media / work / …)
indexa export ~/code/myproject --category code > code-only.xml

# Combine them: code you touched this sprint
indexa export ~/code/myproject --changed-since 14d --category code > sprint.xml
```

Both reuse what's already indexed (recorded mtimes, the classification table) — no re-scan. A
slice that matches nothing exits non-zero with a message rather than writing an empty file, so
it's safe to pipe in a script or CI step.

> **Note:** a slice operates within the exported tree, so a shallow `--depth` clips it — a file
> that matches `--changed-since`/`--category` but sits below the depth cap won't appear. Omit
> `--depth` (full tree, the default) when you want the slice to find matches at any depth.

## 3. Hand it to the AI

- **Claude / claude.ai / Claude Code:** attach or paste `myproject.context.xml`, then ask your
  question. The XML tags help the model navigate the structure.
- **Cursor / Copilot:** add the exported file to the chat context.

## Context Packs — a subject, not a folder

When the files that matter are scattered across the disk (some under `~/code`, some under
`~/Documents`), group them into a named **Context Pack** and export the whole bundle at once:

```bash
indexa pack create api-redesign --description "everything about the v2 API"
indexa pack add api-redesign ~/code/api ~/Documents/specs/api-v2.md ~/notes/api-ideas.md
indexa pack export api-redesign --format xml > api-redesign.xml
```

The pack is a saved, reusable selection — re-export it any time the underlying files change.

## Tips

- Re-running `indexa index` is **incremental** — only changed or newly-unembedded files are
  reprocessed, so refreshing an export before a session is cheap.
- Use `indexa ask --explain "<question>"` first to sanity-check that retrieval surfaces the right
  files before you spend an AI tool's tokens.
