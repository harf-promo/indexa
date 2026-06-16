//! MCP (Model Context Protocol) server exposing the Indexa index to AI agents.
//!
//! Started via `indexa mcp`, it speaks JSON-RPC over **stdio** so clients like
//! Claude Desktop and Cursor can browse the local index live as tool calls. It
//! reuses the existing `Store` and `query` functions directly — no HTTP layer.
//!
//! **stdout is the protocol channel** — all logging must go to stderr.
//!
//! The authoritative tool list is `golden_tools.txt` (enforced by the contract tests
//! below — `tool_contract_golden_list` fails on any add/remove/rename, and
//! `doc_tool_count_matches_code` keeps the counts in README/CLAUDE.md/docs honest).
//! Tool families: retrieval (`search`, `browse_tree`, `get_summary` l0/l1/l2,
//! `read_file`, `ask`), code graph (`dependencies`, `who_imports`, `who_calls`,
//! `blast_radius`, `code_graph`, `related_files`), Context Packs, Smart
//! classification, Importance weighting, saved searches, Insights, decision
//! review (the Decision Ledger), and admin (`get_stats`, `prune`,
//! `trigger_index`).

mod admin;
mod curation;
mod graph;
mod insights;
mod packs;
mod query_extras;
mod retrieval;
mod review;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool_handler, ErrorData, ServerHandler, ServiceExt,
};

use indexa_core::{
    config::{Config, HybridMode},
    store::Store,
};
use indexa_embed::Embedder;
use indexa_llm::Generator;

pub use admin::TriggerIndexParams;
pub use curation::{
    ConfirmClassificationParams, DeleteWeightParams, IgnoreClassificationParams,
    ListClassificationsParams, ListFilesByCategoryParams, SetWeightParams,
};
pub use graph::{
    BlastRadiusParams, CodeGraphParams, DependenciesParams, RelatedFilesParams, WhoCallsParams,
    WhoImportsParams,
};
pub use insights::{InsightsDaysParams, InsightsDuplicatesParams, InsightsLargestParams};
pub use packs::{
    CreatePackMcpParams, DeletePackMcpParams, ExportPackParams, GetPackParams, PackPathsParams,
    SearchPackParams,
};
pub use query_extras::{ExplainRetrievalParams, InspectPathParams, ProjectOverviewParams};
pub use retrieval::{
    AskParams, BrowseParams, GetChunkContextParams, GetSummaryParams, ReadFileParams, SearchParams,
};
pub use review::{
    AnswerDecisionParams, DecisionHistoryParams, DismissDecisionParams, GetDecisionParams,
    ListOpenDecisionsParams,
};

/// Max bytes returned by `read_file` (L2 raw content).
const READ_FILE_CAP: usize = 40 * 1024;

/// The Indexa MCP server handler. Holds only `Send + Sync` state. Each tool opens
/// its own short-lived `Store` connection (a rusqlite `Connection` is `Send` but
/// not `Sync`, so it can't be shared across the async tool futures) — mirroring
/// how the CLI commands each open the store. Connection open is cheap.
#[derive(Clone)]
pub struct IndexaMcp {
    db_path: Arc<PathBuf>,
    embedder: Arc<dyn Embedder + Send + Sync>,
    llm: Arc<dyn Generator + Send + Sync>,
    config: Arc<Config>,
}

fn mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn ok_text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(s.into())])
}

/// Best-effort token-savings telemetry — a recording failure must never fail
/// the user's call, so this swallows errors at debug level instead of `?`.
fn record_usage(store: &mut Store, tool: &str, bytes_served: usize, bytes_counterfactual: u64) {
    if let Err(e) = store.record_tool_usage("mcp", tool, bytes_served as u64, bytes_counterfactual)
    {
        tracing::debug!("usage telemetry skipped ({tool}): {e:#}");
    }
}

impl IndexaMcp {
    pub fn new(
        db_path: PathBuf,
        embedder: Arc<dyn Embedder + Send + Sync>,
        llm: Arc<dyn Generator + Send + Sync>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            db_path: Arc::new(db_path),
            embedder,
            llm,
            config,
        }
    }

    /// Open a fresh read connection to the index (cheap; avoids sharing a
    /// non-`Sync` rusqlite handle across the async tool futures).
    fn store(&self) -> Result<Store, ErrorData> {
        Store::open(&self.db_path).map_err(mcp_err)
    }

    /// Composed router over every tool family module — the single source of
    /// truth for the tool surface, used by both the `#[tool_handler]` dispatch
    /// below and the contract tests.
    pub(crate) fn tool_router() -> ToolRouter<Self> {
        Self::router_retrieval()
            + Self::router_graph()
            + Self::router_packs()
            + Self::router_curation()
            + Self::router_review()
            + Self::router_insights()
            + Self::router_admin()
            + Self::router_query_extras()
    }
}

#[tool_handler(router = Self::tool_router())]
impl ServerHandler for IndexaMcp {
    fn get_info(&self) -> ServerInfo {
        // Identify as "indexa" (from_build_env() bakes in rmcp's own name/version).
        let mut server_info = Implementation::from_build_env();
        server_info.name = "indexa".to_owned();
        server_info.version = env!("CARGO_PKG_VERSION").to_owned();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_instructions(
            "Indexa is a local context engine: a hierarchically-summarized index of your files. \
             Navigate with `browse_tree` and `search`; call `get_summary` with tier=l0 (one-line \
             abstract) to scan cheaply, then drill to l1 (full summary) or l2 (raw content). \
             Use `read_file` for raw text; `ask` for grounded RAG answers (supports scope + mode). \
             Use `trigger_index` to index new or changed files. \
             Context Packs: `list_packs`/`get_pack`/`create_pack`/`add_pack_paths`/\
`remove_pack_paths`/`delete_pack`/`export_pack`/`search_pack` — \
             named, cross-directory bundles ready to paste into any AI tool. \
             Smart classification: `list_classifications`/`confirm_classification`/\
`ignore_classification`. \
             Code graph: `dependencies`/`who_imports`/`who_calls`/`blast_radius`/`code_graph`. \
             Decision review: `list_open_decisions`/`get_decision`/`answer_decision`/\
`dismiss_decision`/`decision_history` — questions Indexa needs a human judgment on; \
             relay them to your user and answer on their behalf."
                .to_owned(),
        )
    }
}

/// Run the Indexa MCP server over stdio until the client disconnects.
///
/// Logging must already be configured to stderr by the caller — stdout is the
/// JSON-RPC channel.
pub async fn serve_mcp(
    db_path: PathBuf,
    embedder: Arc<dyn Embedder + Send + Sync>,
    llm: Arc<dyn Generator + Send + Sync>,
    config: Config,
) -> Result<()> {
    let handler = IndexaMcp::new(db_path, embedder, llm, Arc::new(config));
    let service = handler.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Parse a user-supplied mode string into a `HybridMode`.
/// Accepts `"sparse"`, `"dense"`, `"rrf"` (default).
fn parse_hybrid_mode(s: Option<&str>) -> HybridMode {
    match s.unwrap_or("rrf").to_lowercase().as_str() {
        "sparse" => HybridMode::Sparse,
        "dense" => HybridMode::Dense,
        _ => HybridMode::Rrf,
    }
}

fn xml_escape_mcp(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// True if `requested` lies within any of the (canonicalized) indexed `roots`.
/// Uses component-wise [`Path::starts_with`], so the root `/home/u/proj` does NOT match
/// `/home/u/proj-evil` (a plain string-prefix check would wrongly accept it).
fn path_within_roots(requested: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| requested.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_core::walker::{Entry, EntryKind};
    use rmcp::handler::server::wrapper::Parameters;

    #[test]
    fn path_within_roots_confines_to_index() {
        let roots = vec![PathBuf::from("/home/u/proj"), PathBuf::from("/data/notes")];
        // Inside a root → allowed.
        assert!(path_within_roots(
            Path::new("/home/u/proj/src/a.rs"),
            &roots
        ));
        assert!(path_within_roots(Path::new("/data/notes/x.md"), &roots));
        assert!(path_within_roots(Path::new("/home/u/proj"), &roots));
        // Outside every root → rejected.
        assert!(!path_within_roots(Path::new("/etc/passwd"), &roots));
        assert!(!path_within_roots(Path::new("/home/u/secret.txt"), &roots));
        // Sibling that merely shares a string prefix → rejected (component-wise match).
        assert!(!path_within_roots(Path::new("/home/u/proj-evil/x"), &roots));
        // No indexed roots → nothing is readable.
        assert!(!path_within_roots(Path::new("/home/u/proj/a"), &[]));
    }

    // ── Tool wiring tests (real IndexaMcp against a temp on-disk index) ──

    struct StubEmbedder;
    #[async_trait::async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 8])
        }
        fn dim(&self) -> usize {
            8
        }
    }
    struct StubGenerator;
    #[async_trait::async_trait]
    impl Generator for StubGenerator {
        async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok("stub".to_owned())
        }
    }

    /// An `IndexaMcp` over a fresh temp-file index. Returns the handle plus the `TempDir`
    /// guard for the DB (kept alive by the caller) and a closure-free seeded store.
    fn mcp_with_db(dbdir: &tempfile::TempDir) -> IndexaMcp {
        let dbpath = dbdir.path().join("idx.db");
        // Touch the store so the file + schema exist before the tools open it.
        let _ = Store::open(&dbpath).unwrap();
        IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        )
    }

    #[tokio::test]
    async fn read_file_rejects_path_outside_indexed_roots() {
        // Indexed root with one file inside it…
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("inside.txt");
        std::fs::write(&inside, "hello inside").unwrap();
        // …and a file in a *separate* tree that is NOT indexed.
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();

        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            // Insert only the file; its parent dir (the root) is not itself an entry, so
            // `root_paths()` reports the root — mirroring a real scan.
            store
                .upsert_entries(&[Entry {
                    path: inside.clone(),
                    kind: EntryKind::File,
                    size: 11,
                    modified: None,
                    hint: None,
                }])
                .unwrap();
        }
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );

        // A file inside the indexed root is readable.
        assert!(mcp
            .read_file_inner(inside.to_str().unwrap(), "read_file")
            .is_ok());
        // A file outside every indexed root is rejected (the security contract).
        let err = mcp
            .read_file_inner(outside.to_str().unwrap(), "read_file")
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("not within an indexed root"),
            "expected an indexed-root rejection, got: {err:?}"
        );
    }

    // ── Contract tests: the MCP tool surface is a published API ──

    /// Golden tool list: any added/removed/renamed tool must be a deliberate,
    /// reviewable diff of `golden_tools.txt`. Regenerate with
    /// `INDEXA_UPDATE_GOLDEN=1 cargo test -p indexa-mcp`.
    #[test]
    fn tool_contract_golden_list() {
        let mut names: Vec<String> = IndexaMcp::tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        let actual = names.join("\n") + "\n";

        let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden_tools.txt");
        if std::env::var("INDEXA_UPDATE_GOLDEN").is_ok() {
            std::fs::write(&golden_path, &actual).unwrap();
            return;
        }
        let golden = std::fs::read_to_string(&golden_path)
            .expect(
                "crates/mcp/golden_tools.txt missing — INDEXA_UPDATE_GOLDEN=1 cargo test -p indexa-mcp",
            )
            // A Windows checkout can materialize the file with CRLF; the contract
            // is the tool list, not the line endings (.gitattributes also pins LF).
            .replace("\r\n", "\n");
        assert_eq!(
            actual, golden,
            "MCP tool surface changed. If intentional: INDEXA_UPDATE_GOLDEN=1 cargo test -p indexa-mcp, \
             commit golden_tools.txt, and update the tool counts in README.md / CLAUDE.md / \
             docs/how-to/live-retrieval-over-mcp.md (doc_tool_count_matches_code enforces them)."
        );
    }

    /// Every tool must carry a non-empty description — agents pick tools by it.
    #[test]
    fn every_tool_has_a_description() {
        for tool in IndexaMcp::tool_router().list_all() {
            let desc = tool.description.as_deref().unwrap_or("");
            assert!(
                !desc.trim().is_empty(),
                "tool '{}' has no description",
                tool.name
            );
        }
    }

    /// Extract every "<N> tools" count from a doc body (digits immediately
    /// preceding the literal " tools"; prose like "AI tools" has none and is skipped).
    fn tool_counts_in(text: &str) -> Vec<usize> {
        let bytes = text.as_bytes();
        let mut counts = Vec::new();
        let mut i = 0;
        while let Some(pos) = text[i..].find(" tools") {
            let abs = i + pos;
            let mut start = abs;
            while start > 0 && bytes[start - 1].is_ascii_digit() {
                start -= 1;
            }
            if start < abs {
                counts.push(text[start..abs].parse().unwrap());
            }
            i = abs + " tools".len();
        }
        counts
    }

    /// The "N tools" claims in the docs must equal the real tool count — this is
    /// the guard that retires the "docs said 29, code had 33" drift class.
    #[test]
    fn doc_tool_count_matches_code() {
        let real = IndexaMcp::tool_router().list_all().len();
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for rel in [
            "README.md",
            "CLAUDE.md",
            "docs/how-to/live-retrieval-over-mcp.md",
        ] {
            let text = std::fs::read_to_string(repo.join(rel)).unwrap();
            let counts = tool_counts_in(&text);
            assert!(
                !counts.is_empty(),
                "{rel}: expected at least one '<N> tools' claim (wording changed?)"
            );
            for c in counts {
                assert_eq!(
                    c, real,
                    "{rel} claims {c} MCP tools but the code defines {real} — update the doc"
                );
            }
        }
    }

    /// Golden calls: a few representative tools, end-to-end against a seeded temp
    /// index, asserting the response phrasing agents rely on.
    #[tokio::test]
    async fn contract_golden_calls() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .set_weight("dir", "/proj", 2.0, "user", Some("test"))
                .unwrap();
            store
                .save_query("auth", "where is auth handled?", "rrf", None)
                .unwrap();
        }
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );

        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let stats = text_of(mcp.get_stats().await.unwrap());
        assert!(
            stats.contains("entries") || stats.contains("Entries"),
            "get_stats must report entry counts, got: {stats}"
        );

        let weights = text_of(mcp.list_weights().await.unwrap());
        assert!(
            weights.contains("/proj") && weights.contains("2.0"),
            "list_weights must show the seeded weight, got: {weights}"
        );

        let saved = text_of(mcp.list_saved_queries().await.unwrap());
        assert!(
            saved.contains("auth") && saved.contains("where is auth handled?"),
            "list_saved_queries must show the seeded query, got: {saved}"
        );

        let caveated = text_of(
            mcp.code_graph(Parameters(CodeGraphParams {
                scope: "/proj".into(),
                limit: None,
                strict: false,
            }))
            .await
            .unwrap(),
        );
        assert!(
            caveated.contains("No call edges") || caveated.contains("bare-name"),
            "code_graph must either report emptiness or carry the bare-name caveat, got: {caveated}"
        );
    }

    /// End-to-end over the review family: a seeded open question is listed
    /// with its options, and answering it projects onto the domain tables
    /// (the classification row is the proof the effects actually applied).
    #[tokio::test]
    async fn review_tools_list_and_answer_apply_effects() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        let id = {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .record_decision(indexa_core::store::NewDecision {
                    decision_type: "classification".to_owned(),
                    subject: "/r/proj".to_owned(),
                    params: serde_json::json!({"category": "code", "confidence": 0.7}),
                    options: serde_json::json!(["work", "code", "ignore"]),
                    auto_value: Some("code".to_owned()),
                    confidence: Some(0.7),
                    evidence_hash: "fp1".to_owned(),
                    priority: 50,
                    paths: vec!["/r/proj".to_owned()],
                })
                .unwrap()
                .unwrap()
        };
        let mcp = IndexaMcp::new(
            dbpath.clone(),
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let listed = text_of(
            mcp.list_open_decisions(Parameters(ListOpenDecisionsParams {
                decision_type: None,
                limit: None,
                offset: None,
            }))
            .await
            .unwrap(),
        );
        assert!(
            listed.contains(&format!("#{id}")) && listed.contains("looks like code"),
            "list_open_decisions must show the seeded question, got: {listed}"
        );
        assert!(
            listed.contains("ignore — Ignore (stop suggesting)"),
            "options must render as 'value — label' lines, got: {listed}"
        );

        let answered = text_of(
            mcp.answer_decision(Parameters(AnswerDecisionParams {
                id,
                chosen: "work".to_owned(),
            }))
            .await
            .unwrap(),
        );
        assert!(
            answered.contains("classification"),
            "answer_decision must echo the applied effects, got: {answered}"
        );

        // The projection ran: the answer landed in the domain table as 'user'.
        let store = Store::open(&dbpath).unwrap();
        let c = store.classification_for("/r/proj").unwrap().unwrap();
        assert_eq!((c.category.as_str(), c.source.as_str()), ("work", "user"));
    }

    #[tokio::test]
    async fn set_weight_rejects_negative_weight() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .set_weight(Parameters(SetWeightParams {
                target_kind: "file".into(),
                target: "/some/file.rs".into(),
                weight: -0.5,
                reason: None,
            }))
            .await;
        assert!(res.is_err(), "negative weight must be rejected");
    }

    #[tokio::test]
    async fn set_weight_accepts_valid_weight() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .set_weight(Parameters(SetWeightParams {
                target_kind: "file".into(),
                target: "/some/file.rs".into(),
                weight: 2.0,
                reason: Some("important".into()),
            }))
            .await;
        assert!(res.is_ok(), "a non-negative weight must be accepted");
    }

    #[tokio::test]
    async fn create_pack_rejects_duplicate_name() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let first = mcp
            .create_pack(Parameters(CreatePackMcpParams {
                name: "docs".into(),
                description: None,
            }))
            .await;
        assert!(first.is_ok(), "first create_pack should succeed");
        let dup = mcp
            .create_pack(Parameters(CreatePackMcpParams {
                name: "docs".into(),
                description: None,
            }))
            .await;
        assert!(dup.is_err(), "duplicate pack name must be rejected");
    }
}
