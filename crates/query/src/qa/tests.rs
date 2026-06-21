use super::agentic::parse_followup;
use super::mmr::apply_mmr;
use super::retrieve::{
    apply_archive_penalty, apply_code_intent_boost, cap_per_file, common_ancestor, is_code_intent,
    path_is_historical, truncate_on_boundary,
};
use super::synthesize::{
    build_prompt, pack_context, render_history_block, split_history_budget, trim_continuation,
};
use super::PriorTurn;
use super::*;
use anyhow::Result;
use indexa_core::config::HybridMode;
use indexa_core::store::{SearchHit, Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;
use std::collections::HashMap;

#[test]
fn pack_context_truncates_to_budget() {
    let hits: Vec<SearchHit> = (0..5)
        .map(|i| SearchHit {
            chunk_id: i,
            entry_path: format!("/doc{i}.md"),
            seq: 0,
            heading: String::new(),
            text: "a".repeat(1000),
            rrf_score: 1.0 / (i as f64 + 1.0),
        })
        .collect();

    let (ctx, sources) = pack_context(&hits, "", 2000);
    assert!(ctx.len() <= 2100);
    assert!(!sources.is_empty());
    // The over-budget chunk is cut and explicitly marked so the synthesizer knows.
    assert!(ctx.contains("truncated"));
    // Regression: chunk 0 fits, chunk 1 is truncated but keeps its full `[2]` header,
    // so the model can legitimately cite [2]. Every `[N]` header present in the context
    // MUST have a matching SourceCitation, or the citation dangles (resolves to nothing
    // in every surface that renders `sources` as [1..len]).
    assert!(
        ctx.contains("[2]"),
        "truncated chunk should still carry its [2] header"
    );
    assert_eq!(
        ctx.matches("] /doc").count(),
        sources.len(),
        "every numbered [N] header must have a matching source — no dangling citations"
    );
    assert_eq!(
        sources.len(),
        2,
        "doc0 (full) + doc1 (truncated) are both cited"
    );
}

#[test]
fn build_prompt_contains_question_and_context() {
    let prompt = build_prompt("what is 2+2?", "some context", "");
    assert!(prompt.contains("what is 2+2?"));
    assert!(prompt.contains("some context"));
    // v0.29: the prompt must forbid the model from continuing with another question.
    assert!(prompt.contains("Do not invent or answer any other question"));
}

fn hit(path: &str, score: f64) -> SearchHit {
    SearchHit {
        chunk_id: 1,
        entry_path: path.to_owned(),
        seq: 0,
        heading: String::new(),
        text: "x".to_owned(),
        rrf_score: score,
    }
}

#[test]
fn code_intent_boost_lifts_implementation_over_docs() {
    // Docs outrank code by raw score; a code-intent question must flip that so the
    // implementing file (not the README) answers "which function…".
    let mut hits = vec![
        hit("/docs/readme.md", 1.0),
        hit("/crates/query/src/qa.rs", 0.8),
    ];
    apply_code_intent_boost(
        &mut hits,
        "which function implements archive down-weighting?",
    );
    hits.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap());
    assert_eq!(hits[0].entry_path, "/crates/query/src/qa.rs");

    // A prose question gets no boost — the doc stays on top.
    let mut prose = vec![
        hit("/docs/readme.md", 1.0),
        hit("/crates/query/src/qa.rs", 0.8),
    ];
    apply_code_intent_boost(&mut prose, "what is this project about?");
    prose.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap());
    assert_eq!(prose[0].entry_path, "/docs/readme.md");
}

#[test]
fn is_code_intent_detects_code_questions_only() {
    assert!(is_code_intent("which function does this?"));
    assert!(is_code_intent("how does apply_archive_penalty work")); // snake_case symbol
    assert!(is_code_intent("where is the retrieve method"));
    assert!(!is_code_intent("what is the marketing strategy?"));
    assert!(!is_code_intent("summarize the quarterly results"));
}

/// The default historical-segment set (mirrors `indexa_core::config::default_archive_segments`),
/// used by the archive-penalty tests below.
fn default_segments() -> Vec<String> {
    indexa_core::config::default_archive_segments()
}

#[test]
fn path_is_historical_is_segment_bounded() {
    let seg = default_segments();
    assert!(path_is_historical("/p/docs/archive/known-issues.md", &seg));
    assert!(path_is_historical("/p/historical/x.md", &seg));
    assert!(path_is_historical("/p/Deprecated/y.rs", &seg)); // case-insensitive
                                                             // Not a full segment → not historical (must not over-match).
    assert!(!path_is_historical("/p/src/archived_data.rs", &seg));
    assert!(!path_is_historical("/p/src/threshold.rs", &seg));
    // Empty segment list ⇒ nothing is historical (penalty disabled).
    assert!(!path_is_historical("/p/docs/archive/x.md", &[]));
}

#[test]
fn archive_penalty_demotes_historical_below_current() {
    // Equal raw scores; after the penalty the current doc must rank above the archived one.
    let mut hits = vec![
        hit("/p/docs/archive/known-issues-v0.2.2.md", 1.0),
        hit("/p/CHANGELOG.md", 0.9),
    ];
    apply_archive_penalty(
        &mut hits,
        None,
        &default_segments(),
        indexa_core::config::DEFAULT_ARCHIVE_PENALTY,
    );
    hits.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap());
    assert_eq!(
        hits[0].entry_path, "/p/CHANGELOG.md",
        "current doc must win"
    );
    assert!(
        hits[1].rrf_score < 0.2,
        "archived hit pushed down (0.15×1.0)"
    );
}

#[test]
fn archive_penalty_skipped_when_scoped_into_archive() {
    // If the user explicitly asks within the archive, don't penalize it.
    let mut hits = vec![hit("/p/docs/archive/old.md", 1.0)];
    apply_archive_penalty(
        &mut hits,
        Some("/p/docs/archive"),
        &default_segments(),
        indexa_core::config::DEFAULT_ARCHIVE_PENALTY,
    );
    assert_eq!(
        hits[0].rrf_score, 1.0,
        "scoped-into-archive query keeps full score"
    );
}

#[test]
fn archive_penalty_zero_disables_down_weighting() {
    // penalty = 0.0 turns the feature off: a historical hit keeps its full score.
    let mut hits = vec![hit("/p/docs/archive/old.md", 1.0)];
    apply_archive_penalty(&mut hits, None, &default_segments(), 0.0);
    assert_eq!(
        hits[0].rrf_score, 1.0,
        "penalty 0.0 must leave the historical hit's score unchanged"
    );
}

#[test]
fn archive_penalty_honors_custom_segments() {
    // A user-added segment ("legacy") is penalized when present in the configured list…
    let segments = vec!["legacy".to_owned()];
    let mut hits = vec![hit("/p/legacy/old.md", 1.0)];
    apply_archive_penalty(&mut hits, None, &segments, 0.15);
    assert!(
        hits[0].rrf_score < 0.2,
        "custom 'legacy' segment must be penalized when configured"
    );
    // …but a path under the DEFAULT "archive" segment is untouched when it's not in the
    // custom list (the list fully drives which segments count).
    let mut other = vec![hit("/p/docs/archive/old.md", 1.0)];
    apply_archive_penalty(&mut other, None, &segments, 0.15);
    assert_eq!(
        other[0].rrf_score, 1.0,
        "a segment absent from the configured list is not penalized"
    );
}

#[test]
fn trim_continuation_cuts_invented_turn() {
    // The exact failure shape observed live: the model appended a fabricated next turn.
    let raw = "The project documents known issues.\n\n\n\nQUESTION: what should you do when \
                   contributing?\n\nANSWER: Fork the repo and open a PR.";
    let cut = trim_continuation(raw);
    assert_eq!(cut, "The project documents known issues.");
    assert!(!cut.contains("QUESTION"));
    // A clean single answer is unchanged (just trimmed).
    assert_eq!(
        trim_continuation("  Indexa is a context engine.  "),
        "Indexa is a context engine."
    );
}

#[test]
fn trim_continuation_keeps_legit_inline_question_word() {
    // Conversational prompts now contain a Q:/A: transcript, so the model is more likely
    // to write "Question:" mid-sentence. Only a LINE-LEADING marker is a continuation;
    // an inline mention must survive untouched.
    let s = "The function answers the user's Question: header in the request and returns it.";
    assert_eq!(trim_continuation(s), s);
}

// ── Conversational Ask: history block budgeting ────────────────────────────

fn turn(q: &str, a: &str) -> PriorTurn {
    PriorTurn {
        question: q.to_owned(),
        answer: a.to_owned(),
    }
}

#[test]
fn render_history_block_empty_when_no_turns_or_no_budget() {
    assert_eq!(render_history_block(&[], 1000), "");
    assert_eq!(render_history_block(&[turn("q", "a")], 0), "");
}

#[test]
fn render_history_block_keeps_recent_turns_chronologically() {
    let history = vec![turn("first?", "one"), turn("second?", "two")];
    let block = render_history_block(&history, 1000);
    assert!(block.starts_with("CONVERSATION SO FAR"));
    let first = block.find("first?").unwrap();
    let second = block.find("second?").unwrap();
    assert!(first < second, "turns must be chronological (oldest first)");
}

#[test]
fn split_history_budget_drops_oldest_and_leaves_room_for_chunks() {
    // Three big turns against a small budget: the block is clamped to ≤25%, dropping the
    // OLDEST turns, and the chunk budget keeps most of the budget.
    let big = "x".repeat(2000);
    let history = vec![
        turn("oldest", &big),
        turn("middle", &big),
        turn("newest", &big),
    ];
    let budget = 8000;
    let (block, chunk_budget) = split_history_budget(&history, budget);
    assert!(!block.is_empty());
    // History clamped to ~25% of budget.
    assert!(
        block.len() <= budget * 25 / 100 + 100,
        "block too large: {}",
        block.len()
    );
    // The newest turn is always kept; the oldest is dropped to fit.
    assert!(block.contains("newest"));
    assert!(
        !block.contains("oldest"),
        "oldest turn should be dropped to fit budget"
    );
    // Chunks still get the bulk of the budget.
    assert!(chunk_budget >= budget - budget * 25 / 100 - 100);
}

#[test]
fn build_prompt_includes_history_block_and_guidance() {
    let block = render_history_block(&[turn("what is RRF?", "reciprocal rank fusion")], 1000);
    let prompt = build_prompt("how is it tuned?", "ctx", &block);
    // The rendered block header (distinct from the static instruction prose that always
    // mentions a "CONVERSATION SO FAR block").
    assert!(prompt.contains("CONVERSATION SO FAR (for reference"));
    assert!(prompt.contains("reciprocal rank fusion"));
    assert!(prompt.contains("how is it tuned?"));
    // Empty history ⇒ no rendered conversation block (single-shot prompt unchanged in spirit).
    let plain = build_prompt("q", "ctx", "");
    assert!(!plain.contains("CONVERSATION SO FAR (for reference"));
}

// ── assess_confidence (retrieval-shape classifier) ─────────────────────────

/// Minimal hit with a given fused score (the classifier only reads scores).
fn scored_hit(i: i64, score: f64) -> SearchHit {
    SearchHit {
        chunk_id: i,
        entry_path: format!("/doc{i}.md"),
        seq: 0,
        heading: String::new(),
        text: "x".to_owned(),
        rrf_score: score,
    }
}

fn scored_hits(scores: &[f64]) -> Vec<SearchHit> {
    scores
        .iter()
        .enumerate()
        .map(|(i, s)| scored_hit(i as i64, *s))
        .collect()
}

#[test]
fn confidence_empty_pool_is_none() {
    // The zero-hit short-circuit owns that message; no confidence label on it.
    assert!(assess_confidence(&[], 8, 60.0, true).is_none());
}

#[test]
fn confidence_one_weak_hit_is_low() {
    // Single hit deep in one list: 1/(60+40), below the strong floor 1/70.
    let r = assess_confidence(&scored_hits(&[0.010]), 8, 60.0, true).unwrap();
    assert_eq!(r.level, Confidence::Low);
    assert!(r.basis.contains("may not cover"), "basis: {}", r.basis);
    assert_eq!(r.inputs.strong_hits, 0);
    assert!(r.uncovered.is_none(), "phase-2 placeholder stays None");
}

#[test]
fn confidence_many_strong_corroborated_is_high() {
    // 8 hits, top at 2/(61) (rank-1 in both lists), pool above the strong floor.
    let r = assess_confidence(
        &scored_hits(&[0.0328, 0.0301, 0.028, 0.020, 0.018, 0.016, 0.015, 0.0148]),
        8,
        60.0,
        true,
    )
    .unwrap();
    assert_eq!(r.level, Confidence::High);
    assert_eq!(r.basis, "8 strong matches");
    assert!(r.inputs.gap <= 3.0);
}

#[test]
fn confidence_moderate_uncorroborated_pool_is_medium() {
    // Decent single-list hits but no chunk both retrievers agree on near the top.
    let r = assess_confidence(
        &scored_hits(&[0.0164, 0.0158, 0.0150, 0.0145]),
        8,
        60.0,
        true,
    )
    .unwrap();
    assert_eq!(r.level, Confidence::Medium);
    assert_eq!(r.basis, "4 moderate matches");
}

#[test]
fn confidence_single_strong_hit_caps_at_medium() {
    // One chunk of evidence is never High, however well it scored.
    let r = assess_confidence(&scored_hits(&[0.033]), 8, 60.0, true).unwrap();
    assert_eq!(r.level, Confidence::Medium);
    assert!(r.basis.contains("single"), "basis: {}", r.basis);
}

#[test]
fn confidence_dominant_top_over_weak_pool_is_medium() {
    // gap > 3 (e.g. a weight-boosted top): the pool's strength is illusory.
    let r = assess_confidence(
        &scored_hits(&[0.050, 0.0145, 0.0144, 0.0143, 0.001, 0.001]),
        8,
        60.0,
        true,
    )
    .unwrap();
    assert_eq!(r.level, Confidence::Medium);
    assert_eq!(r.basis, "one dominant match, weak support");
}

#[test]
fn confidence_all_weak_pool_is_low() {
    // Plenty of hits, none reaching top-10-of-a-list mass.
    let r = assess_confidence(
        &scored_hits(&[0.012, 0.011, 0.011, 0.010, 0.010, 0.009]),
        8,
        60.0,
        true,
    )
    .unwrap();
    assert_eq!(r.level, Confidence::Low);
    assert!(r.basis.contains("may not cover"), "basis: {}", r.basis);
}

#[test]
fn confidence_sparse_clean_rank1_counts_without_corroboration() {
    // Sparse-only can't corroborate; a clean keyword rank-1 top still qualifies.
    let r = assess_confidence(
        &scored_hits(&[0.0164, 0.0161, 0.0156, 0.0152, 0.0149, 0.0147]),
        8,
        60.0,
        false,
    )
    .unwrap();
    assert_eq!(r.level, Confidence::High);
    assert!(!r.inputs.embeddings);
}

#[test]
fn confidence_is_order_independent() {
    // Reranked (reordered) hits must classify identically: scores are sorted internally.
    let asc = scored_hits(&[0.0148, 0.016, 0.020, 0.0328, 0.028, 0.018, 0.015, 0.0301]);
    let r = assess_confidence(&asc, 8, 60.0, true).unwrap();
    assert_eq!(r.level, Confidence::High);
    assert_eq!(r.inputs.top_score, 0.0328);
}

// ── answer() unified-pipeline tests (CLI/web/MCP all call this) ────────────
use indexa_core::store::ChunkRecord;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Embedder that counts calls — lets us assert Sparse mode never embeds.
struct CountingEmbedder {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Embedder for CountingEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![0.1, 0.2, 0.3])
    }
    fn dim(&self) -> usize {
        3
    }
}

/// Generator that counts calls and returns a fixed reply.
struct CountingGen {
    calls: Arc<AtomicUsize>,
    reply: String,
}
#[async_trait::async_trait]
impl Generator for CountingGen {
    async fn generate(&self, _prompt: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.reply.clone())
    }
}

fn temp_index_with_chunk(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let mut store = Store::open(&path).unwrap();
    store
        .upsert_chunks(&[ChunkRecord {
            entry_path: "/doc.md".to_owned(),
            seq: 0,
            heading: String::new(),
            text: text.to_owned(),
            language: None,
            embedding: None,
            embed_model: None,
            content_hash: None,
        }])
        .unwrap();
    (dir, path)
}

#[tokio::test]
async fn answer_empty_hits_short_circuits_without_calling_llm() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    Store::open(&path).unwrap(); // empty index, no chunks

    let gen_calls = Arc::new(AtomicUsize::new(0));
    let embedder = CountingEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let llm = CountingGen {
        calls: gen_calls.clone(),
        reply: "should never be used".to_owned(),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        ..QaConfig::default()
    };

    let ans = answer(&path, &embedder, &llm, "anything", &cfg)
        .await
        .unwrap();
    assert!(ans.answer.contains("indexa deep"));
    assert!(ans.sources.is_empty());
    assert!(
        ans.confidence.is_none(),
        "the no-match short-circuit carries no confidence label"
    );
    assert_eq!(
        gen_calls.load(Ordering::SeqCst),
        0,
        "empty hits must short-circuit before any LLM call"
    );
}

#[tokio::test]
async fn answer_sparse_mode_skips_embedding() {
    let (_dir, path) = temp_index_with_chunk("rustacean ferris crab content");
    let embed_calls = Arc::new(AtomicUsize::new(0));
    let embedder = CountingEmbedder {
        calls: embed_calls.clone(),
    };
    let llm = CountingGen {
        calls: Arc::new(AtomicUsize::new(0)),
        reply: "answer".to_owned(),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        ..QaConfig::default()
    };

    let ans = answer(&path, &embedder, &llm, "ferris", &cfg)
        .await
        .unwrap();
    assert_eq!(
        embed_calls.load(Ordering::SeqCst),
        0,
        "Sparse mode must not embed the query"
    );
    assert_eq!(ans.answer, "answer");
}

#[tokio::test]
async fn answer_synthesizes_from_hits() {
    let (_dir, path) = temp_index_with_chunk("the quick brown fox jumps over");
    let gen_calls = Arc::new(AtomicUsize::new(0));
    let embedder = CountingEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let llm = CountingGen {
        calls: gen_calls.clone(),
        reply: "a synthesized answer".to_owned(),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        ..QaConfig::default()
    };

    let ans = answer(&path, &embedder, &llm, "fox", &cfg).await.unwrap();
    assert_eq!(ans.answer, "a synthesized answer");
    assert!(!ans.sources.is_empty());
    assert_eq!(gen_calls.load(Ordering::SeqCst), 1);
    // One sparse rank-1 chunk: confidence present, capped at Medium (single hit).
    let conf = ans.confidence.expect("hits ⇒ a confidence report");
    assert_eq!(conf.level, Confidence::Medium);
    assert!(!conf.inputs.embeddings, "sparse mode never embedded");
}

/// Generator that streams several fragments (overrides generate_stream) so we can verify
/// answer_stream preserves fragment order and event ordering.
struct StreamingGen;
#[async_trait::async_trait]
impl Generator for StreamingGen {
    async fn generate(&self, _prompt: &str) -> Result<String> {
        Ok("unused".to_owned())
    }
    async fn generate_stream(
        &self,
        _prompt: &str,
        on_fragment: &mut (dyn FnMut(String) + Send),
    ) -> Result<String> {
        let mut full = String::new();
        for part in ["Ferris ", "is the ", "Rust mascot."] {
            on_fragment(part.to_owned());
            full.push_str(part);
        }
        Ok(full)
    }
}

#[tokio::test]
async fn answer_stream_emits_sources_before_fragments_in_order() {
    let (_dir, path) = temp_index_with_chunk("ferris the crab is the rust mascot");
    let embedder = CountingEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        ..QaConfig::default()
    };

    let mut frags = String::new();
    let mut seen_fragment = false;
    let mut sources_before_fragment = true;
    let mut sources_count = None;
    {
        let mut on_chunk = |c: AnswerChunk| match c {
            AnswerChunk::Sources(s) => {
                if seen_fragment {
                    sources_before_fragment = false;
                }
                sources_count = Some(s.len());
            }
            AnswerChunk::Fragment(t) => {
                seen_fragment = true;
                frags.push_str(&t);
            }
            AnswerChunk::Step(..) => unreachable!("one-shot stream emits no Step"),
        };
        let ans = answer_stream(
            &path,
            &embedder,
            &StreamingGen,
            "ferris",
            &cfg,
            &mut on_chunk,
        )
        .await
        .unwrap();
        // Reaching the streamed text (not the no-match message) proves hits matched.
        assert_eq!(ans.answer, "Ferris is the Rust mascot.");
        assert_eq!(ans.sources.len(), 1);
    }
    assert!(
        sources_before_fragment,
        "Sources must be emitted before any fragment"
    );
    assert_eq!(sources_count, Some(1), "one source emitted up front");
    assert_eq!(
        frags, "Ferris is the Rust mascot.",
        "fragments must arrive in order and concatenate to the full answer"
    );
}

#[tokio::test]
async fn answer_stream_no_match_emits_guidance_as_one_fragment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    Store::open(&path).unwrap(); // empty index
    let embedder = CountingEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        ..QaConfig::default()
    };
    let mut frags = String::new();
    let mut sources_len = None;
    {
        let mut on_chunk = |c: AnswerChunk| match c {
            AnswerChunk::Sources(s) => sources_len = Some(s.len()),
            AnswerChunk::Fragment(t) => frags.push_str(&t),
            AnswerChunk::Step(..) => unreachable!("one-shot stream emits no Step"),
        };
        let ans = answer_stream(
            &path,
            &embedder,
            &StreamingGen,
            "anything",
            &cfg,
            &mut on_chunk,
        )
        .await
        .unwrap();
        assert!(ans.answer.contains("indexa deep"));
    }
    assert_eq!(sources_len, Some(0), "empty sources event still emitted");
    assert!(
        frags.contains("indexa deep"),
        "no-match guidance arrives as a fragment"
    );
}

// ── Agentic ask ───────────────────────────────────────────────────────────

/// Generator that returns scripted replies in order (so an agentic-loop test can
/// drive distinct decide/synthesis responses); falls back to "DONE" if exhausted.
struct ScriptedGen {
    replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    calls: Arc<AtomicUsize>,
}
impl ScriptedGen {
    fn new(replies: &[&str], calls: Arc<AtomicUsize>) -> Self {
        Self {
            replies: std::sync::Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
            calls,
        }
    }
}
#[async_trait::async_trait]
impl Generator for ScriptedGen {
    async fn generate(&self, _prompt: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "DONE".to_owned()))
    }
}

fn temp_index_with_chunks(chunks: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let mut store = Store::open(&path).unwrap();
    let records: Vec<ChunkRecord> = chunks
        .iter()
        .map(|(p, text)| ChunkRecord {
            entry_path: (*p).to_owned(),
            seq: 0,
            heading: String::new(),
            text: (*text).to_owned(),
            language: None,
            embedding: None,
            embed_model: None,
            content_hash: None,
        })
        .collect();
    store.upsert_chunks(&records).unwrap();
    (dir, path)
}

#[test]
fn stub_chunks_are_excluded_from_retrieval() {
    use indexa_core::store::is_stub_chunk;
    // Truth table for the shared helper.
    assert!(is_stub_chunk("File: Square44x44Logo.png"));
    assert!(is_stub_chunk("Image: photo.jpg"));
    assert!(is_stub_chunk("Media file: clip.mp4"));
    assert!(!is_stub_chunk("Indexa is the local context engine for AI."));
    // A long line that merely starts with the prefix is real content, not a stub.
    assert!(!is_stub_chunk(&format!("File: {}", "x".repeat(90))));

    // A content-free image stub alongside a real chunk; a query matching both must
    // surface only the real one (filtered in SQL + the retrieve() guard).
    let (_d, path) = temp_index_with_chunks(&[
        ("/icons/logo.png", "File: logo.png"),
        (
            "/docs/brand.md",
            "The logo file is the brand mark used across the app.",
        ),
    ]);
    let store = Store::open(&path).unwrap();
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        top_k: 10,
        ..QaConfig::default()
    };
    let hits = retrieve(&store, "logo", None, &cfg, None).unwrap();
    assert!(!hits.is_empty(), "the real chunk should match 'logo'");
    assert!(
        hits.iter().all(|h| !is_stub_chunk(&h.text)),
        "stub chunk leaked into retrieval: {:?}",
        hits.iter().map(|h| h.text.clone()).collect::<Vec<_>>()
    );
    assert!(hits.iter().any(|h| h.entry_path == "/docs/brand.md"));
}

#[test]
fn scoped_retrieval_limits_to_the_path_prefix() {
    // Two files under different dirs; scoping to one dir must exclude the other.
    let (_d, path) = temp_index_with_chunks(&[
        (
            "/src/auth.rs",
            "authentication token refresh and session handling",
        ),
        ("/docs/auth.md", "authentication overview for end users"),
    ]);
    let store = Store::open(&path).unwrap();
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        top_k: 10,
        scope: Some("/src".to_owned()),
        ..QaConfig::default()
    };
    let hits = retrieve(&store, "authentication", None, &cfg, None).unwrap();
    assert!(
        !hits.is_empty(),
        "scoped query should still match in-scope content"
    );
    assert!(
        hits.iter().all(|h| h.entry_path.starts_with("/src")),
        "out-of-scope chunk leaked: {:?}",
        hits.iter()
            .map(|h| h.entry_path.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn parse_followup_extracts_search_query() {
    assert_eq!(
        parse_followup("SEARCH: error handling").as_deref(),
        Some("error handling")
    );
    assert_eq!(
        parse_followup("search: lowercase ok").as_deref(),
        Some("lowercase ok")
    );
    // Tolerates leading reasoning + markdown noise around the action line.
    assert_eq!(
        parse_followup("Hmm, the auth part is missing.\n**SEARCH:** token refresh").as_deref(),
        Some("token refresh")
    );
}

#[test]
fn parse_followup_done_and_garbage_stop_the_loop() {
    assert_eq!(parse_followup("DONE"), None);
    assert_eq!(parse_followup("I think we have enough.\nDONE."), None);
    assert_eq!(
        parse_followup("SEARCH:"),
        None,
        "empty query is not a follow-up"
    );
    assert_eq!(
        parse_followup("I'm not sure what you mean"),
        None,
        "unparseable reply fails open (stops the loop)"
    );
}

#[tokio::test]
async fn agentic_runs_a_second_hop_and_merges_context() {
    // Two chunks matched by different BM25 terms; the follow-up surfaces the
    // second so the final answer draws on both hops.
    let (_d, path) = temp_index_with_chunks(&[
        ("/a.md", "alpha subsystem overview and design"),
        ("/b.md", "beta subsystem error handling details"),
    ]);
    let gen_calls = Arc::new(AtomicUsize::new(0));
    // Single-word follow-up ("beta") so it matches chunk B regardless of whether
    // the BM25 layer treats a multi-word query as a phrase or an AND.
    let llm = ScriptedGen::new(
        &["SEARCH: beta", "DONE", "Both covered [1][2]."],
        gen_calls.clone(),
    );
    let embedder = CountingEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        max_steps: 3,
        ..QaConfig::default()
    };

    let mut steps: Vec<String> = Vec::new();
    let ans = answer_agentic(&path, &embedder, &llm, "alpha", &cfg, &mut |_i, q| {
        steps.push(q.to_owned())
    })
    .await
    .unwrap();

    assert_eq!(steps, vec!["alpha".to_owned(), "beta".to_owned()]);
    assert_eq!(ans.answer, "Both covered [1][2].");
    assert_eq!(
        ans.sources.len(),
        2,
        "both hops' chunks merged into the pool"
    );
    // decide#1 + decide#2 + synthesis = 3 generations.
    assert_eq!(gen_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn agentic_fails_open_to_single_hop_on_unparseable_decision() {
    let (_d, path) = temp_index_with_chunks(&[("/a.md", "alpha subsystem overview")]);
    let gen_calls = Arc::new(AtomicUsize::new(0));
    // Garbage decide reply ⇒ stop after one hop, then synthesize.
    let llm = ScriptedGen::new(&["uhh not sure", "Answer [1]."], gen_calls.clone());
    let embedder = CountingEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        max_steps: 3,
        ..QaConfig::default()
    };

    let mut hops = 0usize;
    let ans = answer_agentic(&path, &embedder, &llm, "alpha", &cfg, &mut |_i, _q| {
        hops += 1
    })
    .await
    .unwrap();

    assert_eq!(
        hops, 1,
        "unparseable decision degrades to a single retrieval"
    );
    assert_eq!(ans.answer, "Answer [1].");
    assert_eq!(
        gen_calls.load(Ordering::SeqCst),
        2,
        "one decide call + one synthesis"
    );
}

#[tokio::test]
async fn agentic_stream_emits_steps_before_sources_and_answer() {
    let (_d, path) = temp_index_with_chunks(&[
        ("/a.md", "alpha subsystem overview and design"),
        ("/b.md", "beta subsystem error handling details"),
    ]);
    let gen_calls = Arc::new(AtomicUsize::new(0));
    let llm = ScriptedGen::new(
        &["SEARCH: beta", "DONE", "Both covered [1][2]."],
        gen_calls.clone(),
    );
    let embedder = CountingEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let cfg = QaConfig {
        mode: HybridMode::Sparse,
        max_steps: 3,
        ..QaConfig::default()
    };

    let mut step_queries: Vec<String> = Vec::new();
    let mut sources_len: Option<usize> = None;
    let mut frags = String::new();
    let mut order: Vec<&str> = Vec::new();
    let answer = {
        let mut on_chunk = |c: AnswerChunk| match c {
            AnswerChunk::Step(_n, q) => {
                step_queries.push(q);
                order.push("step");
            }
            AnswerChunk::Sources(s) => {
                sources_len = Some(s.len());
                order.push("sources");
            }
            AnswerChunk::Fragment(t) => {
                frags.push_str(&t);
                order.push("fragment");
            }
        };
        answer_agentic_stream(&path, &embedder, &llm, "alpha", &cfg, None, &mut on_chunk)
            .await
            .unwrap()
    };

    assert_eq!(answer.answer, "Both covered [1][2].");
    assert_eq!(frags, "Both covered [1][2].");
    assert_eq!(step_queries, vec!["alpha".to_owned(), "beta".to_owned()]);
    assert_eq!(sources_len, Some(2));
    // Every `step` must arrive before the first `sources`/`fragment`.
    let first_answer = order.iter().position(|k| *k != "step").unwrap();
    assert!(order[..first_answer].iter().all(|k| *k == "step"));
    assert_eq!(order[first_answer], "sources");
}

// ── Phase 2: whole-project synthesis helpers ──────────────────────────────

#[test]
fn is_broad_intent_recognises_project_level_questions() {
    // Positive cases
    assert!(is_broad_intent("what is this project about?"));
    assert!(is_broad_intent("what's this project about"));
    assert!(is_broad_intent("tell me about this project"));
    assert!(is_broad_intent("summarize this project"));
    assert!(is_broad_intent("summarise the project"));
    assert!(is_broad_intent("what are the main themes?"));
    assert!(is_broad_intent("give me a high level overview"));
    assert!(is_broad_intent("what is this repo"));
    assert!(is_broad_intent("what are these documents about"));
    assert!(is_broad_intent("describe the whole project"));
    // Negative cases — specific questions must NOT match
    assert!(!is_broad_intent("which function does the archive penalty?"));
    assert!(!is_broad_intent("what is the Q3 budget?"));
    assert!(!is_broad_intent("where is the qa.rs file?"));
    assert!(!is_broad_intent("how do I run the tests?"));
    assert!(!is_broad_intent("what is 2+2"));
}

#[test]
fn pack_context_overview_prepended_and_inside_budget() {
    let overview = "PROJECT OVERVIEW (directory roll-up summaries — background context; \
                        cite numbered excerpts below for specific claims):\n\
                        myproject: A demo project with slides and documents.\n";
    let hits: Vec<SearchHit> = (0..3)
        .map(|i| SearchHit {
            chunk_id: i,
            entry_path: format!("/doc{i}.md"),
            seq: 0,
            heading: String::new(),
            text: "content chunk".to_owned(),
            rrf_score: 1.0 / (i as f64 + 1.0),
        })
        .collect();

    let budget = 4000;
    let (ctx, sources) = pack_context(&hits, overview, budget);

    // Overview must appear first
    assert!(
        ctx.starts_with("PROJECT OVERVIEW"),
        "overview must be first, got: {ctx}"
    );
    // Chunk citations come after
    assert!(ctx.contains("[1] /doc0.md"), "citations must follow: {ctx}");
    // Total length must respect budget
    assert!(ctx.len() <= budget + 50, "exceeded budget: {}", ctx.len());
    // Sources only contain chunk citations (overview is unnumbered)
    assert!(!sources.is_empty());
}

#[test]
fn pack_context_empty_overview_is_byte_identical_to_no_overview() {
    // When overview is empty, pack_context must produce the same output as before
    // this feature was added — this is the regression guard.
    let hits: Vec<SearchHit> = vec![SearchHit {
        chunk_id: 1,
        entry_path: "/a.rs".to_owned(),
        seq: 0,
        heading: "header".to_owned(),
        text: "implementation".to_owned(),
        rrf_score: 0.5,
    }];
    let (ctx_no_overview, _) = pack_context(&hits, "", 4000);
    // Should start directly with the citation, not a blank line
    assert!(
        !ctx_no_overview.starts_with('\n'),
        "extra leading newline: {ctx_no_overview}"
    );
    assert!(
        ctx_no_overview.contains("[1] /a.rs"),
        "citation present: {ctx_no_overview}"
    );
}

#[test]
fn common_ancestor_finds_shared_prefix() {
    let paths = vec![
        "/home/user/project/src/main.rs".to_owned(),
        "/home/user/project/src/lib.rs".to_owned(),
        "/home/user/project/tests/test.rs".to_owned(),
    ];
    let ancestor = common_ancestor(&paths).unwrap();
    assert_eq!(ancestor, "/home/user/project");
}

#[test]
fn common_ancestor_returns_none_for_empty_input() {
    assert!(common_ancestor(&[]).is_none());
}

#[test]
fn common_ancestor_divergent_absolute_paths_return_empty_string() {
    // Two absolute paths always share the root-level empty segment from split('/'),
    // so common_ancestor returns Some("") rather than None. The caller
    // (build_project_overview) looks up the empty path and finds no dir summary,
    // which is the correct graceful-no-op behaviour.
    let paths = vec!["/home/user/a.rs".to_owned(), "/var/log/b.txt".to_owned()];
    let result = common_ancestor(&paths);
    assert!(
        result == Some(String::new()) || result.is_none(),
        "divergent abs paths should be root-level or None, got: {result:?}"
    );
}

#[test]
fn build_prompt_contains_project_overview_guidance() {
    let prompt = build_prompt("what is this project about?", "PROJECT OVERVIEW:\nfoo", "");
    // The prompt must explain the PROJECT OVERVIEW block to the model
    assert!(
        prompt.contains("PROJECT OVERVIEW"),
        "prompt must mention PROJECT OVERVIEW, got: {prompt}"
    );
    // v0.29 anti-continuation guard must be preserved
    assert!(
        prompt.contains("Do not invent or answer any other question"),
        "v0.29 anti-continuation guard removed: {prompt}"
    );
    // Archive/current-sources preference must be preserved
    assert!(
        prompt.contains("Prefer current sources"),
        "archive preference instruction removed: {prompt}"
    );
}

#[test]
fn truncate_on_boundary_respects_utf8() {
    // "こんにちは" is 5 chars × 3 bytes = 15 bytes. Capping at 9 chars must land
    // on a char boundary (not in the middle of a multi-byte sequence).
    let s = "こんにちは world";
    let result = truncate_on_boundary(s, 6);
    // Must be valid UTF-8 (would panic on invalid slice otherwise)
    assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    assert!(result.len() <= s.len());
}

// ── MMR (Maximal Marginal Relevance) re-ranking ───────────────────────────

fn make_hit(id: i64, score: f64) -> SearchHit {
    SearchHit {
        chunk_id: id,
        entry_path: format!("/doc{id}.md"),
        seq: 0,
        heading: String::new(),
        text: "x".to_owned(),
        rrf_score: score,
    }
}

#[test]
fn mmr_with_identical_chunks_demotes_second() {
    // Two hits share the same embedding (cosine sim = 1.0).
    // A third hit is orthogonal (cosine sim = 0.0) but has a lower raw score.
    //
    // With lambda=0.5:
    //   First pick  = A (highest relevance; no selected yet, so no diversity penalty).
    //   MMR(C | A)  = 0.5*0.5  - 0.5*0.0 =  0.25   (orthogonal → zero penalty)
    //   MMR(B | A)  = 0.5*0.8  - 0.5*1.0 = -0.1    (identical  → max penalty)
    //   → Second pick must be C, third must be B.
    let hit_a = make_hit(1, 0.9);
    let hit_b = make_hit(2, 0.8); // identical embedding to A
    let hit_c = make_hit(3, 0.5); // lower raw score but maximally diverse

    let mut embeddings: HashMap<i64, Vec<f32>> = HashMap::new();
    embeddings.insert(1, vec![1.0, 0.0, 0.0]);
    embeddings.insert(2, vec![1.0, 0.0, 0.0]); // identical to A → max penalty
    embeddings.insert(3, vec![0.0, 1.0, 0.0]); // orthogonal to A → zero penalty

    let result = apply_mmr(vec![hit_a, hit_b, hit_c], &embeddings, 0.5);

    assert_eq!(
        result[0].chunk_id, 1,
        "first pick must be highest-relevance hit"
    );
    assert_eq!(
        result[1].chunk_id, 3,
        "diverse hit (C) must beat identical hit (B) in second position"
    );
    assert_eq!(result[2].chunk_id, 2, "identical hit (B) must be last");
}

#[test]
fn mmr_lambda_1_0_is_unchanged() {
    // lambda=1.0 is the "pure relevance / MMR disabled" case.
    // apply_mmr must early-return without reordering the input.
    let hits = vec![make_hit(10, 0.9), make_hit(20, 0.6), make_hit(30, 0.3)];

    let mut embeddings: HashMap<i64, Vec<f32>> = HashMap::new();
    embeddings.insert(10, vec![1.0, 0.0]);
    embeddings.insert(20, vec![1.0, 0.0]); // identical — would be demoted if lambda < 1
    embeddings.insert(30, vec![0.0, 1.0]);

    let result = apply_mmr(hits, &embeddings, 1.0);

    // Order must be byte-for-byte identical to the input.
    assert_eq!(result[0].chunk_id, 10);
    assert_eq!(result[1].chunk_id, 20);
    assert_eq!(result[2].chunk_id, 30);
}

#[test]
fn mmr_fewer_than_two_candidates_is_a_noop() {
    // Nothing to re-order with 0 or 1 candidate — even at a diversifying lambda.
    let mut embeddings: HashMap<i64, Vec<f32>> = HashMap::new();
    embeddings.insert(1, vec![1.0, 0.0]);

    assert!(apply_mmr(vec![], &embeddings, 0.5).is_empty());

    let one = apply_mmr(vec![make_hit(1, 0.9)], &embeddings, 0.5);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].chunk_id, 1);
}

#[test]
fn mmr_without_embeddings_preserves_relevance_order() {
    // No vectors to compute similarity with ⇒ fail open: keep the input order.
    let hits = vec![make_hit(1, 0.9), make_hit(2, 0.6), make_hit(3, 0.3)];
    let empty: HashMap<i64, Vec<f32>> = HashMap::new();

    let result = apply_mmr(hits, &empty, 0.5);

    assert_eq!(result[0].chunk_id, 1);
    assert_eq!(result[1].chunk_id, 2);
    assert_eq!(result[2].chunk_id, 3);
}

// ── GraphRAG-lite: cap_per_file (v0.69) ────────────────────────────────────────
// Distinct chunk_id per hit so order/permutation are observable; path picks the file bucket.
fn fh(chunk_id: i64, path: &str) -> SearchHit {
    SearchHit {
        chunk_id,
        entry_path: path.to_owned(),
        seq: 0,
        heading: String::new(),
        text: String::new(),
        rrf_score: 0.0,
    }
}

#[test]
fn cap_per_file_identity_when_cap_zero() {
    let hits = vec![fh(0, "/a"), fh(1, "/a"), fh(2, "/b")];
    let out = cap_per_file(hits.clone(), 0);
    let ids: Vec<i64> = out.iter().map(|h| h.chunk_id).collect();
    assert_eq!(ids, vec![0, 1, 2], "cap=0 must return the input untouched");
}

#[test]
fn cap_per_file_identity_when_pool_not_larger_than_cap() {
    let hits = vec![fh(0, "/a"), fh(1, "/a")];
    let out = cap_per_file(hits, 5);
    let ids: Vec<i64> = out.iter().map(|h| h.chunk_id).collect();
    assert_eq!(ids, vec![0, 1], "hits.len() <= cap is a no-op");
}

#[test]
fn cap_per_file_front_is_file_diverse_tail_keeps_overflow() {
    // [A0,A1,A2,B0,B1] cap=2 → front takes 2/file in first-appearance order [A0,A1,B0,B1],
    // overflow appended (not dropped): [A0,A1,B0,B1,A2].
    let hits = vec![
        fh(0, "/a"),
        fh(1, "/a"),
        fh(2, "/a"),
        fh(3, "/b"),
        fh(4, "/b"),
    ];
    let ids: Vec<i64> = cap_per_file(hits, 2).iter().map(|h| h.chunk_id).collect();
    assert_eq!(ids, vec![0, 1, 3, 4, 2]);
}

#[test]
fn cap_per_file_preserves_within_file_order() {
    // cap=1: front [A0,B0], overflow tail [A1,A2] in arrival order → A's chunks stay 0,1,2 ordered.
    let hits = vec![fh(0, "/a"), fh(1, "/a"), fh(2, "/a"), fh(3, "/b")];
    let out = cap_per_file(hits, 1);
    let a_ids: Vec<i64> = out
        .iter()
        .filter(|h| h.entry_path == "/a")
        .map(|h| h.chunk_id)
        .collect();
    assert_eq!(a_ids, vec![0, 1, 2], "within-file order must be preserved");
}

#[test]
fn cap_per_file_is_a_permutation_never_drops_a_hit() {
    // The safety lock: the output is a permutation of the input — same length, same multiset of
    // chunk_ids — for any cap. A mis-fired guard can only reorder, never lose a hit.
    for cap in [0usize, 1, 2, 3, 10] {
        let hits = vec![
            fh(0, "/a"),
            fh(1, "/b"),
            fh(2, "/a"),
            fh(3, "/c"),
            fh(4, "/a"),
            fh(5, "/b"),
        ];
        let n = hits.len();
        let out = cap_per_file(hits, cap);
        assert_eq!(out.len(), n, "cap={cap}: length must be preserved");
        let mut got: Vec<i64> = out.iter().map(|h| h.chunk_id).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![0, 1, 2, 3, 4, 5],
            "cap={cap}: id multiset must be preserved"
        );
    }
}
