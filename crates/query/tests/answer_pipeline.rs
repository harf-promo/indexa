//! End-to-end integration test for the unified `query::answer()` pipeline — the single
//! Send-safe entry point that the CLI, web `/api/ask`, and the MCP `ask` tool all call.
//!
//! This is a black-box test: it uses only the public crate APIs (`indexa_query::answer`,
//! `indexa_core::store::Store`, the `Embedder`/`Generator` traits) over a real temp SQLite
//! index with stub models. It stands in for the cross-surface behavior — if this passes, all
//! three surfaces share the same retrieve → (rerank) → synthesize semantics.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use indexa_core::config::HybridMode;
use indexa_core::store::{ChunkRecord, Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;
use indexa_query::{answer, answer_with_ann_history, PriorTurn, QaConfig};

const DIM: usize = 4;

/// Embedder returning a fixed vector; counts calls so tests can assert when the query is embedded.
struct StubEmbedder {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![0.5; DIM])
    }
    fn dim(&self) -> usize {
        DIM
    }
}

/// Generator returning a fixed reply; counts calls so tests can assert the LLM was (not) invoked.
struct StubGenerator {
    reply: String,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Generator for StubGenerator {
    async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.reply.clone())
    }
}

/// Build a temp on-disk index (a real file so `answer` can re-open it) with the given chunks.
fn build_index(dir: &Path, chunks: &[(&str, usize, &str, Option<Vec<f32>>)]) -> std::path::PathBuf {
    let path = dir.join("index.db");
    let mut store = Store::open(&path).unwrap();
    let recs: Vec<ChunkRecord> = chunks
        .iter()
        .map(|(p, seq, text, emb)| ChunkRecord {
            entry_path: (*p).to_owned(),
            seq: *seq,
            heading: String::new(),
            text: (*text).to_owned(),
            language: None,
            embedding: emb.clone(),
            embed_model: None,
            content_hash: None,
        })
        .collect();
    store.upsert_chunks(&recs).unwrap();
    path
}

#[tokio::test]
async fn rrf_pipeline_synthesizes_with_sources() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_index(
        dir.path(),
        &[
            (
                "/a.md",
                0,
                "ferris the crab loves rust",
                Some(vec![0.5; DIM]),
            ),
            (
                "/b.md",
                0,
                "unrelated content about cooking",
                Some(vec![0.1; DIM]),
            ),
        ],
    );
    let embed_calls = Arc::new(AtomicUsize::new(0));
    let gen_calls = Arc::new(AtomicUsize::new(0));
    let embedder = StubEmbedder {
        calls: embed_calls.clone(),
    };
    let llm = StubGenerator {
        reply: "ferris loves rust".to_owned(),
        calls: gen_calls.clone(),
    };
    let cfg = QaConfig {
        mode: HybridMode::Rrf,
        // Isolate the retrieve → synthesize path. Rerank is on by default since v0.44; with the
        // LLM backend it would issue a second StubGenerator call, tripping the "one synthesis
        // call" assertion below. Reranking has its own coverage in `rerank.rs`.
        rerank: false,
        ..QaConfig::default()
    };

    let ans = answer(&path, &embedder, &llm, "rust", &cfg).await.unwrap();
    assert_eq!(ans.answer, "ferris loves rust");
    assert!(!ans.sources.is_empty(), "RRF should produce cited sources");
    assert_eq!(
        embed_calls.load(Ordering::SeqCst),
        1,
        "RRF embeds the query once"
    );
    assert_eq!(gen_calls.load(Ordering::SeqCst), 1, "one synthesis call");
}

#[tokio::test]
async fn empty_index_short_circuits_without_llm() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_index(dir.path(), &[]);
    let gen_calls = Arc::new(AtomicUsize::new(0));
    let embedder = StubEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let llm = StubGenerator {
        reply: "unused".to_owned(),
        calls: gen_calls.clone(),
    };

    let ans = answer(&path, &embedder, &llm, "anything", &QaConfig::default())
        .await
        .unwrap();
    assert!(ans.answer.contains("indexa deep"));
    assert!(ans.sources.is_empty());
    assert_eq!(
        gen_calls.load(Ordering::SeqCst),
        0,
        "empty index must not reach the LLM"
    );
}

#[tokio::test]
async fn dense_mode_retrieves_by_embedding() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_index(
        dir.path(),
        &[("/vec.md", 0, "dense vector match", Some(vec![0.5; DIM]))],
    );
    let embedder = StubEmbedder {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let llm = StubGenerator {
        reply: "matched".to_owned(),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let cfg = QaConfig {
        mode: HybridMode::Dense,
        ..QaConfig::default()
    };

    let ans = answer(&path, &embedder, &llm, "vector", &cfg)
        .await
        .unwrap();
    assert_eq!(ans.answer, "matched");
    assert!(ans.sources.iter().any(|s| s.path.contains("/vec.md")));
}

// ── Conversational Ask (history threading) ─────────────────────────────────

/// With prior turns, the pipeline rewrites the follow-up (1 LLM call) then synthesizes
/// (1 LLM call) → exactly 2 generate calls, and embeds the rewritten query once. With no
/// history the rewrite is skipped entirely (1 generate, the single-shot fast path).
#[tokio::test]
async fn history_triggers_one_rewrite_then_synthesis() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_index(
        dir.path(),
        &[(
            "/a.md",
            0,
            "ferris the crab loves rust",
            Some(vec![0.5; DIM]),
        )],
    );
    let cfg = QaConfig {
        mode: HybridMode::Rrf,
        rerank: false,
        ..QaConfig::default()
    };

    // No history → one synthesis call only.
    let embed0 = Arc::new(AtomicUsize::new(0));
    let gen0 = Arc::new(AtomicUsize::new(0));
    let ans0 = answer_with_ann_history(
        &path,
        &StubEmbedder {
            calls: embed0.clone(),
        },
        &StubGenerator {
            reply: "rust answer".to_owned(),
            calls: gen0.clone(),
        },
        "what is rust",
        &cfg,
        None,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(ans0.answer, "rust answer");
    assert_eq!(
        gen0.load(Ordering::SeqCst),
        1,
        "no history ⇒ no rewrite call"
    );
    assert_eq!(embed0.load(Ordering::SeqCst), 1);

    // With history → rewrite + synthesis = two generate calls.
    let embed1 = Arc::new(AtomicUsize::new(0));
    let gen1 = Arc::new(AtomicUsize::new(0));
    let history = vec![PriorTurn {
        question: "tell me about ferris".to_owned(),
        answer: "ferris is the rust mascot".to_owned(),
    }];
    let ans1 = answer_with_ann_history(
        &path,
        &StubEmbedder {
            calls: embed1.clone(),
        },
        &StubGenerator {
            reply: "rust answer".to_owned(),
            calls: gen1.clone(),
        },
        "and what does it love?",
        &cfg,
        None,
        &history,
    )
    .await
    .unwrap();
    assert_eq!(ans1.answer, "rust answer");
    assert_eq!(
        gen1.load(Ordering::SeqCst),
        2,
        "history ⇒ one rewrite + one synthesis"
    );
    assert_eq!(
        embed1.load(Ordering::SeqCst),
        1,
        "rewritten query embedded once"
    );
}

/// A multi-turn synthesis whose model output runs on into a fabricated next turn is still
/// trimmed (the in-prompt Q:/A: transcript makes this MORE likely, not less).
#[tokio::test]
async fn history_answer_trims_hallucinated_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_index(
        dir.path(),
        &[(
            "/a.md",
            0,
            "ferris the crab loves rust",
            Some(vec![0.5; DIM]),
        )],
    );
    let cfg = QaConfig {
        mode: HybridMode::Rrf,
        rerank: false,
        ..QaConfig::default()
    };
    let history = vec![PriorTurn {
        question: "what is ferris".to_owned(),
        answer: "the rust mascot".to_owned(),
    }];
    // The rewrite call also returns this string, but clean_rewrite takes the first line
    // ("Ferris loves rust.") as the standalone query — harmless for retrieval.
    let llm = StubGenerator {
        reply: "Ferris loves rust.\nQUESTION: invented follow-up?\nANSWER: nope".to_owned(),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let ans = answer_with_ann_history(
        &path,
        &StubEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        },
        &llm,
        "does it love rust?",
        &cfg,
        None,
        &history,
    )
    .await
    .unwrap();
    assert_eq!(ans.answer, "Ferris loves rust.");
    assert!(
        !ans.answer.contains("invented"),
        "hallucinated turn must be trimmed"
    );
}
