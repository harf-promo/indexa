#!/usr/bin/env python3
"""Repo-local PostToolUse(Edit|Write|MultiEdit) auto-reindex for the indexa repo.

SILENT + BACKGROUNDED. When a file under this repo is edited, kick off an
incremental `indexa index <file>` in the background so the local context store
stays fresh without manual `trigger_index` calls. Wired in `.claude/settings.json`
so it loads only in this repo (it replaces the former user-scope hook).

Safeguards:
  - The repo root comes from $CLAUDE_PROJECT_DIR (fallback: two levels above this
    file). Only edits under that root fire; everything else exits immediately.
  - Linked git worktrees (`.git` is a file, not a directory) are skipped, so a
    scratch worktree never writes its absolute paths into the shared index.
  - Debounced: at most one reindex per DEBOUNCE_S seconds (marker file mtime), so
    a burst of edits does not thrash Ollama.
  - `indexa index` is refresh-aware and scoped to the single edited path.
  - Fail-open: a missing binary or any error exits 0 silently. Never blocks,
    never prints.

`indexa` is found on PATH first, then at ~/.cargo/bin/indexa.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

DEBOUNCE_S = 90
FALLBACK_BIN = os.path.expanduser("~/.cargo/bin/indexa")


def main():
    try:
        _run()
    except Exception:
        pass
    sys.exit(0)


def _project_root():
    env = os.environ.get("CLAUDE_PROJECT_DIR")
    if env:
        return os.path.realpath(env)
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.realpath(os.path.join(here, "..", ".."))


def _run():
    data = json.load(sys.stdin)
    if data.get("tool_name") not in ("Edit", "Write", "MultiEdit"):
        return

    fp = (data.get("tool_input") or {}).get("file_path", "") or ""
    if not fp:
        return
    fp = os.path.realpath(os.path.abspath(fp))

    root = _project_root()
    # Fast early-exit: only edits inside this repo matter.
    if os.path.commonpath([fp, root]) != root:
        return
    # Primary checkout only: a linked worktree has a `.git` file, not a directory.
    if not os.path.isdir(os.path.join(root, ".git")):
        return

    marker = os.path.join(
        tempfile.gettempdir(),
        "indexa-reindex-%s" % hashlib.sha1(root.encode()).hexdigest()[:12],
    )
    now = time.time()
    try:
        if now - os.path.getmtime(marker) < DEBOUNCE_S:
            return
    except OSError:
        pass  # no marker yet: proceed

    indexa = shutil.which("indexa") or (
        FALLBACK_BIN if os.path.exists(FALLBACK_BIN) else None
    )
    if not indexa:
        return  # fail-open: binary not found

    # Touch the marker before launching so concurrent edits debounce correctly.
    try:
        with open(marker, "w") as f:
            f.write(str(now))
    except OSError:
        pass

    devnull = subprocess.DEVNULL
    subprocess.Popen(
        [indexa, "index", fp],
        stdout=devnull, stderr=devnull, stdin=devnull,
        start_new_session=True,
        cwd=root,
    )


if __name__ == "__main__":
    main()
