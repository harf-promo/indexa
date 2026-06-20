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
```

### Regression gate (compare against a baseline)

`--min-hit-rate` is an *absolute* floor. To catch a *relative* slip — "this change dropped MRR
0.74 → 0.61" — save a baseline run and compare against it. This is how a retrieval change (new
chunker, reranker, ranking tweak) proves it didn't regress:

```bash
indexa eval golden.json --json > baseline.json        # snapshot the current quality
# … make your retrieval change, rebuild …
indexa eval golden.json --baseline baseline.json                    # print per-metric deltas
indexa eval golden.json --baseline baseline.json --max-regression 0.02   # exit 1 if any metric drops > 0.02
```

`--baseline` prints a `vs baseline:` line with the signed delta for every aggregate metric; with
`--max-regression <d>` (default `0.0` = no drop allowed) it exits 1 if hit_rate, MRR, recall, nDCG,
or precision falls more than `d` below the baseline. The baseline file is either a full
`indexa eval --json` output or a bare summary object. (Sub-noise jitter from the JSON round-trip is
ignored — only real drops count.)

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
the dense path. Note that sparse retrieval treats a multi-word question as a phrase, the same as
`ask --sparse-only` — write sparse golden questions as phrases that actually occur in the content,
or expect (and track) the miss.
