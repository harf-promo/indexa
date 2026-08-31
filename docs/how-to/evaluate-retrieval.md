# Evaluate retrieval quality

**Goal:** regression-test retrieval with a golden-questions file, so a change to chunking,
parsing, ranking, or reranking can't silently make `ask`/`search` worse. `indexa eval` runs each
question through the same retrieval the `ask` pipeline uses and scores the ranked hits — **no LLM
synthesis, ever, by default**. By default reranking is excluded too (eval stays LLM-free and
hermetic, so rerank-enabled configs diverge by exactly that step), and in sparse mode (the
default) no embedder is needed either. Pass `--rerank` to also route retrieval through the SAME
rerank pass `ask` uses (needs a local LLM, or the cross-encoder model when
`[retrieval] rerank_backend = "cross-encoder"`).

Everything above scores *ranking* — which chunks came back. Pass `--judge` to additionally grade
the *answer text* a real `ask` would synthesize from them; see [`--judge` mode](#--judge-mode-grade-the-synthesized-answer)
below. `--judge` is the one thing in this command that isn't hermetic — never add it to a
required/blocking CI job (see that section).

## The golden file

A JSON file listing questions and the paths a correct retrieval must surface (as stored in the
index: absolute, tilde allowed):

```json
{
  "questions": [
    {
      "question": "where is auth handled?",
      "expect_paths": ["~/code/myrepo/src/auth.rs", "~/code/myrepo/src/session.rs"]
    },
    {
      "question": "how is the connection pool configured?",
      "expect_paths": ["~/code/myrepo/src/db.rs"],
      "k": 5
    }
  ]
}
```

- `expect_paths` — a hit on **any** of them counts; list every acceptable file. An **absolute**
  path (tilde allowed) must match exactly. A **relative** path (no leading `/`) matches as a
  path-boundary suffix of the stored absolute path — so a fixture committed to a repo (e.g.
  `crates/query/src/eval.rs`) matches wherever the repo is checked out, on CI or any machine.
- `k` *(optional)* — per-question cutoff; defaults to the run-level `--top-k` (10).

## Running it

```bash
indexa eval golden.json                          # sparse (default) — hermetic, CI-safe
indexa eval golden.json --mode rrf               # hybrid, needs the embedder used at index time
indexa eval golden.json --scope ~/code/myrepo    # confine retrieval to one tree
indexa eval golden.json --json | jq .summary     # machine output
indexa eval golden.json --min-hit-rate 0.8       # exit 1 below 80% hit rate (absolute floor)
indexa eval golden.json --rerank                 # also score ask's rerank pass — needs a local LLM
```

### Regression gate (compare against a baseline)

`--min-hit-rate` is an *absolute* floor. To catch a *relative* slip — "this change dropped MRR
0.74 → 0.61" — save a baseline run and compare against it. This is how a retrieval change (new
chunker, reranker, ranking tweak) proves it didn't regress:

```bash
indexa eval golden.json --save-run ~/indexa-eval-runs   # snapshot the current quality, dated
# … make your retrieval change, rebuild …
indexa eval golden.json --baseline ~/indexa-eval-runs/eval-1735689600.json
indexa eval golden.json --baseline ~/indexa-eval-runs/eval-1735689600.json --max-regression 0.02
```

`--save-run <dir>` is the recommended way to produce a `--baseline` file: it writes this run's
`{mode, questions, summary}` payload (the same shape `--json` prints) to `<dir>/eval-<unix>.json`,
creating `<dir>` if needed, and prints the path it wrote. It's a first-class replacement for
manually redirecting `--json` output (`indexa eval golden.json --json > baseline.json` still works
and needs no extra flags, but `--save-run` gives every run its own timestamped file instead of one
you have to remember to rename before the next snapshot) — and it works standalone, so you don't
also need to pass `--json`. It composes with `--baseline`, `--rerank`, and `--judge` in the same
run: pass `--save-run` and `--baseline` together to both check against an old baseline *and* record
today's run as tomorrow's.

`--baseline` prints a `vs baseline:` line with the signed delta for every aggregate metric; with
`--max-regression <d>` (default `0.0` = no drop allowed) it exits 1 if hit_rate, MRR, recall, nDCG,
or precision falls more than `d` below the baseline. The baseline file is either a full
`indexa eval --json` output (including one written by `--save-run`) or a bare summary object.
(Sub-noise jitter from the JSON round-trip is ignored — only real drops count.)

## The metrics

| Metric | Per question | Aggregate |
|---|---|---|
| **hit@k** | any expected path in the top k | fraction of questions that hit (`hit_rate`) |
| **MRR** | 1 / rank of the first expected path (0 on a miss) | mean reciprocal rank (`mrr`) |
| **recall@k** | fraction of the *distinct expected paths* covered in the top k | mean (`mean_recall`) |
| **nDCG@k** | binary-relevance nDCG — how high the expected hits rank (1.0 = packed at top) | mean (`mean_ndcg`) |
| **citation precision** | fraction of returned hits whose path is expected | mean (`mean_precision`) |

`hit@k` only asks *"any expected path?"*; **recall@k** grades *"how many of them?"* (a 2-path
question with one retrieved scores 0.5), and **nDCG@k** catches a *ranking* regression — an expected
hit sliding from #1 to #6 — that `hit@k` is blind to.

Sample output:

```
hit  rank      rr   prec    rec  ndcg  question
  ✓     1   1.000   0.50   1.00  1.00  where is auth handled?
  ✗     -   0.000   0.00   0.00  0.00  how is the connection pool configured?

2 questions · hit rate 0.50 · MRR 0.500 · recall 0.50 · nDCG 0.500 · precision 0.25 · mode sparse
```

Exit code is 0 unless the aggregate hit rate drops below `--min-hit-rate` (default 0, i.e. report
only). In CI, index hermetically with **`indexa deep --no-embed`** — an FTS-only pass that skips the
Ollama preflight and every model call, so it needs no models pulled and no network:

```bash
indexa scan .
indexa deep . --no-embed                                   # FTS-only; no Ollama
indexa eval fixtures/self-golden.json --mode sparse --min-hit-rate <baseline>
```

(Plain `indexa deep` requires a reachable embedder — `--no-embed` is what makes the gate hermetic;
dense/hybrid retrieval needs a later embedded `deep`.) Indexa runs exactly this on itself: the
`retrieval eval (self-golden, hermetic)` CI job scores [`fixtures/self-golden.json`](../../fixtures/self-golden.json)
on every PR.

Sparse mode scores BM25 keyword retrieval only — it tells you nothing about embedding quality.
Use `--mode rrf` locally (with the same embedder the index was built with) when a change touches
the dense path, and add `--rerank` when the change touches reranking (either backend). Note that
sparse retrieval treats a multi-word question as a phrase, the same as `ask --sparse-only` — write
sparse golden questions as phrases that actually occur in the content, or expect (and track) the
miss.

`--mode rrf`/`dense` also unlocks MMR (`retrieve()` skips it entirely in sparse mode), so combining
`--mode rrf --rerank` is the closest this command gets to the full production `ask` ranking. Both
need a real index built with embeddings (`indexa deep` without `--no-embed`) — the hermetic CI gate
deliberately can't run either; see `.github/workflows/dense-eval.yml` (`workflow_dispatch`) for the
one that can.

## `--judge` mode: grade the synthesized answer

Everything above is a *ranking* metric — it tells you whether the right chunks came back, and in
what order. It says nothing about the ANSWER text a real `ask` would produce from them: a retrieval
that hits every expected file can still be synthesized into an answer that misreads a source,
hedges into vagueness, or states something none of the cited chunks actually support. `--judge`
grades that:

```bash
indexa eval golden.json --judge
indexa eval golden.json --judge --judge-model gemma3:12b   # grade with a different model than synthesis
indexa eval golden.json --judge --min-judge-score 3.5      # exit 1 if the mean judge score is below 3.5
```

For each question, `--judge` runs the SAME synthesis entry point `ask` uses
(`qa::answer_with_ann_history` — respecting `--rerank` when both flags are set), then sends the
question, the sources the answer cited, and the answer text to a judge LLM with a fixed rubric:

1. Does the answer directly address the question?
2. Is every factual claim in the answer supported by the cited sources (no invented facts, no
   unsupported claims)?

The judge responds with a 0-5 score and a one-sentence reason. Per-question output gains a line:

```
hit  rank      rr   prec    rec  ndcg  question
  ✓     1   1.000   1.00   1.00  1.00  where is auth handled?
      judge 4/5 — Correctly names the session module; doesn't mention the token refresh path.

1 questions · hit rate 1.00 · MRR 1.000 · recall 1.00 · nDCG 1.00 · precision 1.00 · mode sparse · judge 4.00/5 (1/1 graded)
```

**What's different from the ranking metrics**: hit@k/MRR/recall/nDCG/precision are pure,
deterministic functions of the retrieved path list — same input, same score, every run.
`mean_judge_score` is an LLM's own judgment call on prose it also had to read; it isn't calibrated,
and given the same input it can vary run to run (temperature, model, prompt phrasing). Treat it as
a directional signal for "did this change make answers worse," not a precise number to chase to the
hundredth.

**Cost — NOT hermetic.** Unlike everything above, `--judge` makes two real model calls per question
(one to synthesize the answer, one to grade it) — it needs a reachable local LLM (or whichever
`[describer]` provider is configured) and, in `rrf`/`dense` mode, an embedder too. It is never added
to a required/blocking CI job by Indexa itself — the hermetic `retrieval eval (self-golden,
hermetic)` job never passes `--judge`, and `fixtures/self-golden.json` carries no
`expect_answer_hint` fields. Set it up as your own optional CI job (or a local pre-release check)
once you've picked a judge model you're willing to pay the latency/cost for on every run.

A synthesis or judge-parse failure for one question is logged to stderr and just leaves that
question's `judge` field absent — it does not abort the run. `--min-judge-score` treats "every
question failed to get a verdict" as a failing run too (a gate that can't measure must fail, not
silently pass), the same way a missing/empty index already fails the whole command.

### The `expect_answer_hint` field (optional)

A golden question can optionally carry a short human-written note on what a correct answer should
mention — the judge includes it in the rubric prompt when present:

```json
{
  "question": "how does RRF fusion combine sparse and dense scores?",
  "expect_paths": ["crates/query/src/qa/retrieve.rs"],
  "expect_answer_hint": "should mention the rrf_k rank constant and that scores are summed, not averaged"
}
```

This field is entirely optional and backward compatible — an existing golden file (or one written
without `--judge` in mind, like `fixtures/self-golden.json`) needs no changes; the judge grades
purely on question + sources + answer when it's absent.

### `mean_judge_score` / `judged_questions` in `--json` output and baselines

With `--judge`, the `summary` object gains two fields: `mean_judge_score` (the mean 0-5 score
across questions that got a verdict) and `judged_questions` (how many did). Both are `Option` —
absent from `--json` output on a plain run, and `null`/missing when loading an OLD baseline file
saved before this feature existed, so `--baseline` comparisons against pre-`--judge` snapshots keep
working unchanged. (Note `compare_to_baseline`/`--max-regression` does not currently compare
`mean_judge_score` — it stays a `--min-judge-score` absolute-floor-only gate, like `--min-hit-rate`
was before baselines existed.)
