# Evaluate retrieval quality

**Goal:** regression-test retrieval with a golden-questions file, so a change to chunking,
parsing, or ranking can't silently make `ask`/`search` worse. `indexa eval` runs each question
through the same retrieval the `ask` pipeline uses (with reranking excluded — eval stays LLM-free, so rerank-enabled configs diverge by exactly that step) and scores the ranked hits — **no LLM, no
synthesis**, and in sparse mode (the default) no embedder, so it runs hermetically in CI.

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

- `expect_paths` — a hit on **any** of them counts; list every acceptable file.
- `k` *(optional)* — per-question cutoff; defaults to the run-level `--top-k` (10).

## Running it

```bash
indexa eval golden.json                          # sparse (default) — hermetic, CI-safe
indexa eval golden.json --mode rrf               # hybrid, needs the embedder used at index time
indexa eval golden.json --scope ~/code/myrepo    # confine retrieval to one tree
indexa eval golden.json --json | jq .summary     # machine output
indexa eval golden.json --min-hit-rate 0.8       # exit 1 below 80% hit rate (the CI gate)
```

## The metrics

| Metric | Per question | Aggregate |
|---|---|---|
| **hit@k** | any expected path in the top k | fraction of questions that hit (`hit_rate`) |
| **MRR** | 1 / rank of the first expected path (0 on a miss) | mean reciprocal rank (`mrr`) |
| **citation precision** | fraction of returned hits whose path is expected | mean (`mean_precision`) |

Sample output:

```
hit  rank      rr   prec  question
  ✓     1   1.000   0.50  where is auth handled?
  ✗     -   0.000   0.00  how is the connection pool configured?

2 questions · hit rate 0.50 · MRR 0.500 · precision 0.25 · mode sparse
```

Exit code is 0 unless the aggregate hit rate drops below `--min-hit-rate` (default 0, i.e. report
only). In CI: index the fixture repo (`indexa scan` + `indexa deep --embed-model` skipped — sparse
needs no embeddings), then `indexa eval golden.json --min-hit-rate <baseline>`.

Sparse mode scores BM25 keyword retrieval only — it tells you nothing about embedding quality.
Use `--mode rrf` locally (with the same embedder the index was built with) when a change touches
the dense path. Note that sparse retrieval treats a multi-word question as a phrase, the same as
`ask --sparse-only` — write sparse golden questions as phrases that actually occur in the content,
or expect (and track) the miss.
