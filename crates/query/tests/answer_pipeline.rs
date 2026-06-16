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
use indexa_query::{answer, QaConfig};

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
