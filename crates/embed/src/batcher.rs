//! Cross-file embed-batching accumulator for the deep-index loop.
//!
//! Today each file embeds only its own 1–3 cache-miss chunks, so a deep index issues
//! roughly one Ollama round-trip per file (~100k for 100k files). [`MissBatcher`] buffers
//! cache-MISS embed-texts *across* files so [`embed_all`](crate::embed_all) runs on full
//! [`EMBED_BATCH_SIZE`](crate::EMBED_BATCH_SIZE) batches instead — the batching win, with
//! no change to what is stored.
//!
//! It is deliberately **passive**: it never touches an [`Embedder`](crate::Embedder) or a
//! store. The caller drains it (`batch_refs`), runs `embed_all` itself — so the web deep
//! loop can fire its memory watchdog immediately before the big embed — and hands the
//! results back (`scatter`) to be routed to the owning chunks. A file is returned as
//! [`Completed`] only once every one of *its* misses has been embedded, so the caller can
//! upsert it exactly once, in FIFO order.
//!
//! **Correctness note (why cross-file batching is safe):** a chunk's stored `text` and
//! `content_hash` are pure functions of its raw text (see `chunk_text_for_store` /
//! `chunk_content_hash`), independent of which HTTP call its embed-text rides in. Every
//! retrieval consumer is cosine and scale-invariant (brute-force cosine + the HNSW
//! `AnnIndex`'s `DistCosine`), and the Ollama client already stores a mix of L2-normalized
//! and raw vectors by design that "rank identically" — so reordering which files' misses
//! share a batch is rank-neutral. The bar is retrieval-equivalence, not byte-identity.

use std::collections::HashMap;

/// One buffered miss embed-text, tagged with the file + slot to route its result back to.
struct Queued {
    /// The (possibly contextual-enriched) text to embed.
    text: String,
    /// Which in-flight file this miss belongs to (`Pending` key).
    file_id: usize,
    /// Index into the owning file's `embeddings` vec — i.e. the chunk index.
    slot: usize,
}

/// An in-flight file: buffered but not yet fully embedded. Removed from the batcher the
/// moment its last miss is scattered back.
struct Pending<M> {
    /// Aligned to the file's chunk count; cache-hit slots pre-filled `Some`, miss slots
    /// start `None` and are filled on `scatter`.
    embeddings: Vec<Option<Vec<f32>>>,
    /// Miss slots not yet scattered. The file is COMPLETE when this reaches 0.
    remaining: usize,
    /// Misses whose `embed_all` result came back `None` (embed failed).
    raw_failures: usize,
    /// Misses whose returned vector had the wrong dim (rejected, slot nulled).
    dim_mismatch: usize,
    /// First wrong dim seen, for the aggregate warning text.
    dim_sample: Option<usize>,
    /// Total misses this file had (warning denominator).
    miss_count: usize,
    /// Opaque per-file payload the caller needs at finalize (chunks, hashes, edges, path).
    meta: M,
}

/// A file whose misses are all resolved (or that had none) — ready for the caller to
/// finalize (build records, upsert, emit progress).
pub struct Completed<M> {
    /// Merged, aligned to chunk order: hit slots from cache, miss slots scattered.
    pub embeddings: Vec<Option<Vec<f32>>>,
    /// Count of misses that failed to embed (`None`), attributed to THIS file even though a
    /// flush mixes files.
    pub raw_failures: usize,
    /// Count of misses whose vector was the wrong dim and got nulled, attributed to THIS file.
    pub dim_mismatch: usize,
    /// A sample wrong dim, for the warning message.
    pub dim_sample: Option<usize>,
    /// Total misses this file had.
    pub miss_count: usize,
    /// The opaque payload handed to [`MissBatcher::add_file`].
    pub meta: M,
}

/// Outcome of registering a file with the batcher.
pub enum AddOutcome<M> {
    /// The file had zero misses (all cache hits) — nothing was buffered; finalize it now.
    Complete(Completed<M>),
    /// The file had ≥1 miss and was buffered. The caller should then flush if
    /// [`MissBatcher::is_full`].
    Buffered,
}

/// Cross-file embed-batching accumulator. Generic over an opaque per-file payload `M` so it
/// stays free of any `store`/`parsers` dependency and lives beside `embed_all`.
pub struct MissBatcher<M> {
    /// Configured `[embedding] dim`. Vectors of any other length are rejected on `scatter`
    /// (mirrors [`enforce_embedding_dim`](crate::enforce_embedding_dim), per-vector).
    expected_dim: usize,
    /// Flush threshold — `EMBED_BATCH_SIZE` in production, small in tests.
    batch_size: usize,
    /// Flat FIFO buffer of miss embed-texts awaiting a flush.
    buf: Vec<Queued>,
    /// In-flight files, keyed by a monotonic id. Holds only the ~`batch_size` files whose
    /// misses are currently buffered (entries are removed on completion).
    pending: HashMap<usize, Pending<M>>,
    /// Next `Pending` key.
    next_id: usize,
}

impl<M> MissBatcher<M> {
    /// `expected_dim` = configured `[embedding] dim`; `batch_size` = flush threshold
    /// (`EMBED_BATCH_SIZE`).
    pub fn new(expected_dim: usize, batch_size: usize) -> Self {
        Self {
            expected_dim,
            batch_size: batch_size.max(1),
            buf: Vec::new(),
            pending: HashMap::new(),
            next_id: 0,
        }
    }

    /// Register one file. `embeddings` is presized to the file's chunk count with cache hits
    /// already placed (`Some`) and miss slots `None`. `miss_texts` is `(slot, enriched
    /// embed-text)` per miss. A zero-miss file is finalized synchronously (returned
    /// `Complete`) and never enters the buffer.
    pub fn add_file(
        &mut self,
        embeddings: Vec<Option<Vec<f32>>>,
        miss_texts: Vec<(usize, String)>,
        meta: M,
    ) -> AddOutcome<M> {
        if miss_texts.is_empty() {
            return AddOutcome::Complete(Completed {
                embeddings,
                raw_failures: 0,
                dim_mismatch: 0,
                dim_sample: None,
                miss_count: 0,
                meta,
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        let miss_count = miss_texts.len();
        for (slot, text) in miss_texts {
            self.buf.push(Queued {
                text,
                file_id: id,
                slot,
            });
        }
        self.pending.insert(
            id,
            Pending {
                embeddings,
                remaining: miss_count,
                raw_failures: 0,
                dim_mismatch: 0,
                dim_sample: None,
                miss_count,
                meta,
            },
        );
        AddOutcome::Buffered
    }

    /// True once the buffer holds at least a full batch — the caller should flush.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.batch_size
    }

    /// True when nothing is buffered (skip the tail flush).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Number of miss embed-texts currently buffered.
    #[inline]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Borrow the buffered embed-texts for [`embed_all`](crate::embed_all), in FIFO order.
    /// The caller MUST pass the results straight to [`scatter`](Self::scatter) unchanged —
    /// `embed_all` is order-preserving, so `results[i]` is the embedding of `batch_refs()[i]`.
    pub fn batch_refs(&self) -> Vec<&str> {
        self.buf.iter().map(|q| q.text.as_str()).collect()
    }

    /// Hand back `embed_all` results (same length + order as the last [`batch_refs`]).
    /// Routes each vector to its file+slot (rejecting a wrong-dim vector like
    /// `enforce_embedding_dim` does, per-vector), drains the buffer, and returns every file
    /// now COMPLETE, in FIFO completion order.
    pub fn scatter(&mut self, results: Vec<Option<Vec<f32>>>) -> Vec<Completed<M>> {
        let buf = std::mem::take(&mut self.buf);
        debug_assert_eq!(
            results.len(),
            buf.len(),
            "scatter results must align 1:1 with the last batch_refs()"
        );
        let dim = self.expected_dim;
        let mut done = Vec::new();
        for (q, res) in buf.into_iter().zip(results) {
            let complete = {
                let p = self
                    .pending
                    .get_mut(&q.file_id)
                    .expect("pending file live until its last miss scatters");
                match res {
                    Some(v) if v.len() == dim => p.embeddings[q.slot] = Some(v),
                    Some(v) => {
                        p.dim_sample.get_or_insert(v.len());
                        p.dim_mismatch += 1;
                    }
                    None => p.raw_failures += 1,
                }
                p.remaining -= 1;
                p.remaining == 0
            };
            if complete {
                let p = self.pending.remove(&q.file_id).expect("just checked live");
                done.push(Completed {
                    embeddings: p.embeddings,
                    raw_failures: p.raw_failures,
                    dim_mismatch: p.dim_mismatch,
                    dim_sample: p.dim_sample,
                    miss_count: p.miss_count,
                    meta: p.meta,
                });
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{embed_all, Embedder};
    use std::sync::Mutex;

    /// Deterministic `dim`-length embedding of a text (byte sum in slot 0), so identity is
    /// checkable across batchings.
    fn code(text: &str, dim: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        v[0] = text.bytes().map(u32::from).sum::<u32>() as f32;
        v
    }

    /// Records every `embed_batch` group size, so tests can assert batches actually reach 64.
    /// Text markers force the two failure modes: `FAIL*` → the whole batch errors (→ per-text
    /// fallback, and that text's single `embed` also errors → `None`); `WRONGDIM*` → a vector
    /// of `dim+1` length (accepted by embed, rejected on scatter).
    struct RecordingEmbedder {
        dim: usize,
        batch_sizes: Mutex<Vec<usize>>,
    }

    impl RecordingEmbedder {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                batch_sizes: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Embedder for RecordingEmbedder {
        async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            if text.starts_with("FAIL") {
                anyhow::bail!("forced embed failure for {text}");
            }
            if text.starts_with("WRONGDIM") {
                return Ok(code(text, self.dim + 1));
            }
            Ok(code(text, self.dim))
        }
        fn dim(&self) -> usize {
            self.dim
        }
        async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            self.batch_sizes.lock().unwrap().push(texts.len());
            // A FAIL* text bails the whole group → embed_all falls back to per-text embed
            // (mirroring the real Ollama batch→single fallback).
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
    }

    /// A test file: (cache_hits presized to the chunk count, `(slot, embed-text)` per miss,
    /// meta/tag). Mirrors what the deep loop hands `add_file`.
    type TestFile = (Vec<Option<Vec<f32>>>, Vec<(usize, String)>, String);

    /// Drive files through the batcher exactly as the deep loop will: add each, flush on
    /// `is_full`, tail-flush at the end. Returns the finalized files keyed by `meta`.
    async fn run(
        dim: usize,
        batch_size: usize,
        embedder: &RecordingEmbedder,
        files: Vec<TestFile>,
    ) -> Vec<Completed<String>> {
        let mut b = MissBatcher::<String>::new(dim, batch_size);
        let mut done = Vec::new();
        for (hits, misses, meta) in files {
            match b.add_file(hits, misses, meta) {
                AddOutcome::Complete(c) => done.push(c),
                AddOutcome::Buffered => {}
            }
            if b.is_full() {
                let refs = b.batch_refs();
                let out = embed_all(embedder, &refs, batch_size).await;
                drop(refs);
                done.extend(b.scatter(out));
            }
        }
        if !b.is_empty() {
            let refs = b.batch_refs();
            let out = embed_all(embedder, &refs, batch_size).await;
            drop(refs);
            done.extend(b.scatter(out));
        }
        assert!(b.is_empty(), "buffer must be drained after tail flush");
        done
    }

    /// A file with `n` all-miss chunks: cache_hits all `None`, misses tagged `{tag}-{i}`.
    fn all_miss_file(tag: &str, n: usize) -> TestFile {
        let hits = vec![None; n];
        let misses = (0..n).map(|i| (i, format!("{tag}-{i}"))).collect();
        (hits, misses, tag.to_string())
    }

    // (a) Every finalized chunk's vector matches embedding the miss-text alone — proves
    //     cross-file batching doesn't change any stored vector (given order-preserving embed).
    #[tokio::test]
    async fn record_parity_with_per_text_embedding() {
        let dim = 3;
        let e = RecordingEmbedder::new(dim);
        // Mixed hit/miss files whose misses interleave across flushes at batch_size 4.
        // File B's chunk 0 is a cache hit; chunk 1 is a miss.
        let files = vec![
            all_miss_file("A", 3),
            (
                vec![Some(code("cached", dim)), None],
                vec![(1, "B-1".to_string())],
                "B".to_string(),
            ),
            all_miss_file("C", 2),
        ];
        let done = run(dim, 4, &e, files).await;
        assert_eq!(done.len(), 3);
        for c in &done {
            for (slot, emb) in c.embeddings.iter().enumerate() {
                let expected = match c.meta.as_str() {
                    "A" => Some(code(&format!("A-{slot}"), dim)),
                    "C" => Some(code(&format!("C-{slot}"), dim)),
                    "B" if slot == 0 => Some(code("cached", dim)), // cache hit, untouched
                    "B" => Some(code("B-1", dim)),
                    _ => unreachable!(),
                };
                assert_eq!(emb.as_ref(), expected.as_ref(), "{}[{slot}]", c.meta);
            }
        }
    }

    // (b) With enough single-miss files, a cross-file batch actually reaches batch_size.
    #[tokio::test]
    async fn cross_file_batches_reach_batch_size() {
        let dim = 1;
        let e = RecordingEmbedder::new(dim);
        let files: Vec<_> = (0..100)
            .map(|i| all_miss_file(&format!("f{i}"), 1))
            .collect();
        let done = run(dim, 64, &e, files).await;
        assert_eq!(done.len(), 100);
        let sizes = e.batch_sizes.lock().unwrap().clone();
        assert_eq!(sizes, vec![64, 36], "one full 64 batch then the 36 tail");
    }

    // (c) An all-cache-hit file finalizes with zero embed calls.
    #[tokio::test]
    async fn all_cache_hit_file_needs_no_embedding() {
        let dim = 2;
        let e = RecordingEmbedder::new(dim);
        let hits = vec![Some(code("x", dim)), Some(code("y", dim))];
        let files = vec![(hits, vec![], "hitonly".to_string())];
        let done = run(dim, 64, &e, files).await;
        assert_eq!(done.len(), 1);
        assert!(
            e.batch_sizes.lock().unwrap().is_empty(),
            "no embed round-trip for an all-hit file"
        );
        assert_eq!(done[0].miss_count, 0);
        assert_eq!(
            done[0].embeddings,
            vec![Some(code("x", dim)), Some(code("y", dim))]
        );
    }

    // (d) A file with >64 misses completes across ≥2 internal sub-batches, exactly once.
    #[tokio::test]
    async fn file_with_more_than_batch_size_misses_completes_once() {
        let dim = 1;
        let e = RecordingEmbedder::new(dim);
        let files = vec![all_miss_file("big", 100)];
        let done = run(dim, 64, &e, files).await;
        assert_eq!(done.len(), 1, "the >64-miss file finalizes exactly once");
        assert_eq!(done[0].embeddings.len(), 100);
        assert!(done[0].embeddings.iter().all(|e| e.is_some()));
        assert_eq!(e.batch_sizes.lock().unwrap().clone(), vec![64, 36]);
    }

    // (e) Interleaved misses from 3 files in one flush route back to the right file+slot.
    #[tokio::test]
    async fn scatter_routes_each_vector_to_its_owner() {
        let dim = 2;
        let e = RecordingEmbedder::new(dim);
        // batch_size huge → single tail flush mixes all files' misses.
        let files = vec![
            all_miss_file("A", 2),
            all_miss_file("B", 2),
            all_miss_file("C", 2),
        ];
        let done = run(dim, 1024, &e, files).await;
        assert_eq!(done.len(), 3);
        for c in &done {
            for (slot, emb) in c.embeddings.iter().enumerate() {
                assert_eq!(
                    emb.as_ref(),
                    Some(&code(&format!("{}-{slot}", c.meta), dim)),
                    "no vector leaked across files: {}[{slot}]",
                    c.meta
                );
            }
        }
    }

    // (f) Dim-mismatch and embed-failure counts attribute to the owning file despite a
    //     mixed flush.
    #[tokio::test]
    async fn warnings_reattribute_per_file() {
        let dim = 2;
        let e = RecordingEmbedder::new(dim);
        let clean = all_miss_file("A", 2);
        let wrongdim = (
            vec![None, None],
            vec![(0, "WRONGDIM-0".to_string()), (1, "WRONGDIM-1".to_string())],
            "B".to_string(),
        );
        let failing = (
            vec![None, None],
            vec![(0, "FAIL-0".to_string()), (1, "FAIL-1".to_string())],
            "C".to_string(),
        );
        let done = run(dim, 1024, &e, vec![clean, wrongdim, failing]).await;
        let by = |m: &str| done.iter().find(|c| c.meta == m).unwrap();
        let a = by("A");
        assert_eq!((a.dim_mismatch, a.raw_failures), (0, 0));
        assert!(a.embeddings.iter().all(|e| e.is_some()));
        let b = by("B");
        assert_eq!((b.dim_mismatch, b.raw_failures), (2, 0));
        assert_eq!(b.dim_sample, Some(dim + 1));
        assert!(b.embeddings.iter().all(|e| e.is_none()), "wrong-dim nulled");
        let c = by("C");
        assert_eq!((c.dim_mismatch, c.raw_failures), (0, 2));
        assert!(c.embeddings.iter().all(|e| e.is_none()), "failed nulled");
    }

    // (g) Below-threshold files finalize only at the tail flush.
    #[tokio::test]
    async fn tail_flush_finalizes_buffered_files() {
        let dim = 1;
        let e = RecordingEmbedder::new(dim);
        // 3 single-miss files, batch_size 64 → never is_full → all wait for the tail.
        let files: Vec<_> = (0..3).map(|i| all_miss_file(&format!("t{i}"), 1)).collect();
        let done = run(dim, 64, &e, files).await;
        assert_eq!(done.len(), 3);
        assert_eq!(
            e.batch_sizes.lock().unwrap().clone(),
            vec![3],
            "one tail batch of 3"
        );
    }
}
