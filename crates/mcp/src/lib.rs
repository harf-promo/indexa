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
//! `doc_tool_count_matches_code` keeps the counts in README/AGENTS.md/USAGE.md/docs honest).
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
mod prompts;
mod query_extras;
mod resources;
mod retrieval;
mod review;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{
        CallToolResult, Content, GetPromptRequestParams, GetPromptResult, Implementation,
        ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, ServerCapabilities,
        ServerInfo,
    },
    service::RequestContext,
    tool_handler, ErrorData, RoleServer, ServerHandler, ServiceExt,
};

use indexa_core::{
    config::{Config, HybridMode},
    store::{AnnIndex, Store},
};
use indexa_embed::Embedder;
use indexa_llm::Generator;

pub use admin::TriggerIndexParams;
pub use curation::{
    ConfirmClassificationParams, DeleteWeightParams, IgnoreClassificationParams,
    ListClassificationsParams, ListFilesByCategoryParams, SetWeightParams,
};
pub use graph::{
    BlastRadiusParams, ChangedImpactParams, CodeGraphParams, DependenciesParams,
    DependencyClosureParams, RelatedFilesParams, SymbolContextParams, TracePathParams,
    WhoCallsParams, WhoImportsParams,
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

/// The MCP server's cached ANN index plus the `(chunk_count, max_chunk_id)` watermark it was
/// built at — a mismatch means a `deep`/`trigger_index` changed the chunks table and the index
/// must be rebuilt. Mirrors the web server's `AnnCache` (which is `pub(crate)` to that crate, so
/// it can't be shared here) so MCP — the primary AI surface — gets the same fast dense retrieval.
#[derive(Default)]
struct AnnCache {
    index: Option<Arc<AnnIndex>>,
    watermark: (i64, i64),
}

/// MCP tool-surface profile (3.2) — `core` un-advertises AND un-calls (via rmcp's
/// `disable_route`, not a filtered listing) every tool outside a small task-focused subset,
/// cutting the per-session schema token cost for subagents that only need retrieval + basic
/// graph/pack/decision access. `full` (the default) is byte-identical to pre-3.2 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolProfile {
    #[default]
    Full,
    Core,
}

impl ToolProfile {
    /// Parse a `--tool-profile`/`[mcp] tool_profile` value. Fail-open: anything other than
    /// exactly `"core"` (including unrecognized values) resolves to `Full` — an unrecognized
    /// profile must never accidentally hide tools from an agent that needs them.
    pub fn parse(s: &str) -> Self {
        match s {
            "core" => ToolProfile::Core,
            _ => ToolProfile::Full,
        }
    }
}

/// The `core` profile's tool set — retrieval plus the minimum graph/pack/decision surface for
/// a subagent doing focused, bounded work. Subset-only by construction: every name here must
/// also appear in [`IndexaMcp::tool_router`]'s full set, enforced by
/// `core_profile_is_a_subset_of_the_full_tool_set`.
const CORE_TOOL_NAMES: &[&str] = &[
    "search",
    "ask",
    "dependencies",
    "who_calls",
    "blast_radius",
    "list_packs",
    "search_pack",
    "export_pack",
    "add_note",
    "list_open_decisions",
];

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
    tool_profile: ToolProfile,
    /// Cached HNSW index for dense retrieval, shared across tool calls (the MCP server is
    /// long-lived, so the build cost amortizes). Watermark-keyed so a re-index refreshes it.
    ann: Arc<tokio::sync::RwLock<AnnCache>>,
    /// Single-flight guard so concurrent cold/stale asks don't each build a full index.
    ann_build_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Map an internal failure (store/IO/embedder) to a JSON-RPC internal error (-32603).
/// Uses `{:#}` (the alternate `Display`) so an `anyhow::Error`'s full cause chain reaches the
/// calling agent instead of only the outermost context — e.g. `opening index at /x/idx.db:
/// unable to open database file` rather than just `opening index at /x/idx.db`. For
/// non-anyhow `Display` types the alternate flag is a harmless no-op.
fn mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}

/// Map a caller mistake (an unrecognized enum value, an out-of-range argument) to a JSON-RPC
/// invalid-params error (-32602), so an agent can tell "I called this wrong" apart from "the
/// server broke" — the latter (`mcp_err`) implies retrying the same call won't help.
fn mcp_invalid(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(format!("{e:#}"), None)
}

fn ok_text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(s.into())])
}

/// Best-effort token-savings telemetry — a recording failure must never fail
/// the user's call, so this swallows errors at debug level instead of `?`.
fn record_usage(store: &mut Store, tool: &str, bytes_served: usize, bytes_counterfactual: u64) {
    // MCP calls aren't session-scoped for the savings ledger (the ledger is web-session
    // driven); pass None so these still record into the weekly aggregate.
    if let Err(e) =
        store.record_tool_usage("mcp", tool, bytes_served as u64, bytes_counterfactual, None)
    {
        tracing::debug!("usage telemetry skipped ({tool}): {e:#}");
    }
}

impl IndexaMcp {
    /// `full` tool profile (byte-identical to pre-3.2 behavior). See
    /// [`Self::new_with_profile`] for `--tool-profile core`.
    pub fn new(
        db_path: PathBuf,
        embedder: Arc<dyn Embedder + Send + Sync>,
        llm: Arc<dyn Generator + Send + Sync>,
        config: Arc<Config>,
    ) -> Self {
        Self::new_with_profile(db_path, embedder, llm, config, ToolProfile::Full)
    }

    /// 3.2 — construct with an explicit tool profile.
    pub fn new_with_profile(
        db_path: PathBuf,
        embedder: Arc<dyn Embedder + Send + Sync>,
        llm: Arc<dyn Generator + Send + Sync>,
        config: Arc<Config>,
        tool_profile: ToolProfile,
    ) -> Self {
        Self {
            db_path: Arc::new(db_path),
            embedder,
            llm,
            config,
            tool_profile,
            ann: Arc::new(tokio::sync::RwLock::new(AnnCache::default())),
            ann_build_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Open a fresh read connection to the index (cheap; avoids sharing a
    /// non-`Sync` rusqlite handle across the async tool futures).
    fn store(&self) -> Result<Store, ErrorData> {
        Store::open(&self.db_path).map_err(mcp_err)
    }

    /// Lazily build (and cache) the ANN index for dense retrieval, or return `None` to fall back
    /// to the brute-force cosine scan. `None` when ANN is off (`[retrieval] ann`), the index is
    /// below `ann_min_chunks`, or a build/read fails. Rebuilds when the chunk watermark
    /// `(count, max_chunk_id)` changes, so a `deep`/`trigger_index` that adds or edits chunks
    /// transparently refreshes the index on the next call. Ported from the web server's
    /// `ensure_ann`; all store access uses fresh read connections so nothing is held across the
    /// CPU-heavy build. `ann.as_deref()` at the call sites turns the returned `Arc` into the
    /// `Option<&AnnIndex>` the query pipeline already threads end-to-end.
    async fn ensure_ann(&self) -> Option<Arc<AnnIndex>> {
        if !self.config.retrieval.ann {
            return None;
        }
        let db_path = (*self.db_path).clone();
        let min_chunks = self.config.retrieval.ann_min_chunks;

        // Watermark = (chunk_count, max_chunk_id): AUTOINCREMENT ids are monotonic, so any
        // insert/edit bumps max_id and any delete changes the count — a stale index is always
        // detected.
        let (count, max_id) = tokio::task::spawn_blocking({
            let db_path = db_path.clone();
            move || -> Option<(i64, i64)> {
                let s = Store::open(&db_path).ok()?;
                Some((s.chunk_count().ok()? as i64, s.max_chunk_id().ok()?))
            }
        })
        .await
        .ok()??;

        if (count as usize) < min_chunks {
            return None;
        }

        // Fast path: cached index still matches the watermark.
        {
            let cache = self.ann.read().await;
            if let Some(idx) = &cache.index {
                if cache.watermark == (count, max_id) {
                    return Some(idx.clone());
                }
            }
        }

        // Single-flight: serialize builds; re-check after acquiring (another caller may have just
        // built the current index).
        let _build_guard = self.ann_build_lock.lock().await;
        {
            let cache = self.ann.read().await;
            if let Some(idx) = &cache.index {
                if cache.watermark == (count, max_id) {
                    return Some(idx.clone());
                }
            }
        }

        // Build fresh (CPU-heavy → spawn_blocking; reads on its own connection). Stream the
        // embeddings straight into the HNSW rather than materializing them all in a Vec first
        // (halves transient build memory on a large index). Dim comes from the first non-empty
        // stored vector (all stored vectors share it) via `first_embedding_dim`, matching what the
        // old collect-then-`find` did; the built index indexes exactly the same set either way.
        let built = tokio::task::spawn_blocking(move || -> Option<AnnIndex> {
            let s = Store::open(&db_path).ok()?;
            let dim = s.first_embedding_dim().ok()??;
            let capacity = s.count_embedded_chunks().ok()?;
            let idx = AnnIndex::build_from(dim, capacity, |insert| {
                s.stream_chunk_embeddings(|id, v| insert(id, v))
            })
            .ok()?;
            Some(idx)
        })
        .await
        .ok()??;

        let idx = Arc::new(built);
        {
            let mut cache = self.ann.write().await;
            cache.index = Some(idx.clone());
            cache.watermark = (count, max_id);
        }
        Some(idx)
    }

    /// Composed router over every tool family module — the single source of
    /// truth for the FULL tool surface (contract tests, golden list, doc counts). The
    /// `#[tool_handler]` dispatch below uses [`Self::active_tool_router`] instead, which
    /// applies the instance's profile on top of this.
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

    /// 3.2 — the router that actually serves THIS instance's requests: the full router with
    /// every tool outside the active profile disabled via rmcp's `disable_route` (un-advertised
    /// in `list_tools` AND rejected by `call_tool` — not merely filtered from a listing).
    /// `Full` (the default) is the unmodified full router, so profile-off behavior is
    /// byte-identical to pre-3.2.
    fn active_tool_router(&self) -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        if self.tool_profile == ToolProfile::Core {
            for tool in router.clone().list_all() {
                if !CORE_TOOL_NAMES.contains(&tool.name.as_ref()) {
                    router.disable_route(tool.name);
                }
            }
        }
        router
    }
}

/// The `full` profile's static instructions prose — every tool it names is available under
/// `ToolProfile::Full` (the unrestricted router), so this is safe to keep hand-written.
/// Resources/Prompts are listed in both profiles' instructions because `tool_profile` only
/// gates the `ToolRouter` (tools), never `list_resources`/`list_prompts` — both stay available
/// under `core` too.
const FULL_INSTRUCTIONS: &str =
    "Indexa is a local context engine: a hierarchically-summarized index of your files. \
     Navigate with `browse_tree` and `search`; call `get_summary` with tier=l0 (one-line \
     abstract) to scan cheaply, then drill to l1 (full summary) or l2 (raw content). \
     Use `read_file` for raw text; `ask` for grounded RAG answers (supports scope + mode). \
     NOTE: `ask` synthesizes with Indexa's LOCAL model (e.g. ollama/gemma3:12b), not your \
     model — so if you are a strong model, call `ask` with `synthesize: false` to get the \
     retrieved context slice and write the answer yourself (better, and no local-model cost), \
     or compose `search`/`get_chunk_context`/`export_pack` for raw context. \
     Use `trigger_index` to index new or changed files. \
     Context Packs: `list_packs`/`get_pack`/`create_pack`/`add_pack_paths`/\
`remove_pack_paths`/`delete_pack`/`export_pack`/`search_pack` — \
     named, cross-directory bundles ready to paste into any AI tool. \
     Smart classification: `list_classifications`/`confirm_classification`/\
`ignore_classification`. \
     Code graph: `dependencies`/`who_imports`/`who_calls`/`blast_radius`/`code_graph`. \
     Decision review: `list_open_decisions`/`get_decision`/`answer_decision`/\
`dismiss_decision`/`decision_history` — questions Indexa needs a human judgment on; \
     relay them to your user and answer on their behalf. \
     Resources (`indexa://overview`, `indexa://packs`, `indexa://pack/{name}`, \
`indexa://summary/{path}`) and Prompts (`onboarding-overview`, `explain-file`, \
`pack-context`) expose the same index data for browsing/attachment.";

/// The `core` profile's instructions — built from [`CORE_TOOL_NAMES`] itself (via the
/// trailing "Available tools" sentence) rather than a second hand-maintained prose list, so a
/// future edit to `CORE_TOOL_NAMES` can't silently leave this prose naming a tool the profile
/// no longer exposes. The narrative sentences above it only ever name tools that are also in
/// `CORE_TOOL_NAMES` — `core_instructions_never_names_a_non_core_tool` pins this.
fn core_instructions() -> String {
    format!(
        "Indexa is a local context engine: a hierarchically-summarized index of your files. \
         This server is running the CORE tool profile — a bounded retrieval + graph/pack/\
         decision subset; every other tool is hidden from `tools/list` and rejected if called. \
         Use `search` for keyword+semantic search over indexed content, and `ask` for grounded \
         RAG answers. NOTE: `ask` synthesizes with Indexa's LOCAL model (e.g. ollama/gemma3:12b), \
         not your model — if you are a strong model, call `ask` with `synthesize: false` to get \
         the retrieved context slice and write the answer yourself instead (better, and no \
         local-model cost). Code graph: `dependencies`/`who_calls`/`blast_radius`. Context \
         Packs: `list_packs`/`search_pack`/`export_pack` — named, cross-directory bundles ready \
         to paste into any AI tool; `add_note` writes something you learned back into a pack. \
         Decision review: `list_open_decisions` — questions Indexa needs a human judgment on; \
         relay them to your user and answer on their behalf. \
         Resources (`indexa://overview`, `indexa://packs`, `indexa://pack/{{name}}`, \
`indexa://summary/{{path}}`) and Prompts (`onboarding-overview`, `explain-file`, \
`pack-context`) expose the same index data for browsing/attachment. \
         Available tools in this profile: {}.",
        CORE_TOOL_NAMES.join(", ")
    )
}

#[tool_handler(router = self.active_tool_router())]
impl ServerHandler for IndexaMcp {
    fn get_info(&self) -> ServerInfo {
        // Identify as "indexa" (from_build_env() bakes in rmcp's own name/version).
        let mut server_info = Implementation::from_build_env();
        server_info.name = "indexa".to_owned();
        server_info.version = env!("CARGO_PKG_VERSION").to_owned();
        // 3.2 / Wave 7: the instructions prose must reflect what `self.tool_profile` actually
        // exposes — a `core`-profile caller told about a tool it can't reach (e.g. `read_file`,
        // `code_graph`) will call it and get rejected. See `core_instructions`.
        let instructions = match self.tool_profile {
            ToolProfile::Full => FULL_INSTRUCTIONS.to_owned(),
            ToolProfile::Core => core_instructions(),
        };
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(server_info)
        .with_instructions(instructions)
    }

    // ── Resources (read-only index artifacts) ──────────────────────────────────
    // Hand-written (resources have no router macro); the inner methods live in
    // `resources.rs`. Tools stay the source of truth for the 46-tool golden list —
    // resources/prompts are a separate protocol surface and don't affect it.

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(
            self.list_resources_inner(),
        ))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            self.resource_templates_inner(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        self.read_resource_inner(&request.uri)
    }

    // ── Prompts (reusable, index-backed templates) ─────────────────────────────

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(self.list_prompts_inner()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        self.get_prompt_inner(&request.name, request.arguments.as_ref())
    }
}

/// Run the Indexa MCP server over stdio until the client disconnects.
///
/// Logging must already be configured to stderr by the caller — stdout is the
/// JSON-RPC channel. `tool_profile` is 3.2's `--tool-profile`/`[mcp] tool_profile`
/// surface — `ToolProfile::Full` (the default) is byte-identical to pre-3.2 behavior.
pub async fn serve_mcp(
    db_path: PathBuf,
    embedder: Arc<dyn Embedder + Send + Sync>,
    llm: Arc<dyn Generator + Send + Sync>,
    config: Config,
    tool_profile: ToolProfile,
) -> Result<()> {
    let handler =
        IndexaMcp::new_with_profile(db_path, embedder, llm, Arc::new(config), tool_profile);
    let service = handler.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Parse a user-supplied mode string into a `HybridMode`.
/// `None`, or a blank/whitespace-only string, defaults to `"rrf"` — matching how every other
/// optional string param in this crate treats an empty value as "absent" (e.g. `scope.filter(|s|
/// !s.is_empty())` at each of this function's call sites). A *present, non-blank* but
/// unrecognized value is a caller error — rejected as `invalid_params` — rather than silently
/// coerced to `rrf`; the old silent-fallback behavior meant a typo like `mode:"dnese"` ran a full
/// hybrid search the caller never asked for, with no error signal at all.
fn parse_hybrid_mode(s: Option<&str>) -> Result<HybridMode, ErrorData> {
    match s.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(HybridMode::Rrf),
        Some(v) => match v.to_lowercase().as_str() {
            "sparse" => Ok(HybridMode::Sparse),
            "dense" => Ok(HybridMode::Dense),
            "rrf" => Ok(HybridMode::Rrf),
            other => Err(mcp_invalid(format!(
                "invalid mode '{other}' — expected one of: sparse, dense, rrf"
            ))),
        },
    }
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

    /// An embedder that always fails — for testing an embedder-outage error path
    /// (`search` mode `"dense"`) without needing a real (or missing) Ollama.
    struct FailingEmbedder;
    #[async_trait::async_trait]
    impl Embedder for FailingEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Err(anyhow::anyhow!("embedder offline"))
        }
        fn dim(&self) -> usize {
            8
        }
    }

    /// Concatenate a `CallToolResult`'s text content blocks — the common shape every tool
    /// call's assertions need to inspect.
    fn tool_text(r: CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
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

    #[test]
    fn parse_hybrid_mode_rejects_unknown_values_instead_of_coercing() {
        // A present-but-unrecognized value must be an error, not a silent fallback to `rrf` —
        // the old behavior hid a typo like `mode:"dnese"` behind a full hybrid search the
        // caller never asked for.
        use rmcp::model::ErrorCode;

        assert!(matches!(parse_hybrid_mode(None), Ok(HybridMode::Rrf)));
        assert!(matches!(
            parse_hybrid_mode(Some("rrf")),
            Ok(HybridMode::Rrf)
        ));
        assert!(matches!(
            parse_hybrid_mode(Some("SPARSE")),
            Ok(HybridMode::Sparse)
        ));
        assert!(matches!(
            parse_hybrid_mode(Some("dense")),
            Ok(HybridMode::Dense)
        ));
        // A blank/whitespace-only value is treated as "absent" (defaults to rrf), matching how
        // every other optional string param in this crate treats an empty value — not rejected
        // as an unrecognized enum value.
        assert!(matches!(parse_hybrid_mode(Some("")), Ok(HybridMode::Rrf)));
        assert!(matches!(
            parse_hybrid_mode(Some("   ")),
            Ok(HybridMode::Rrf)
        ));
        // Surrounding whitespace on an otherwise-valid value is trimmed, not rejected.
        assert!(matches!(
            parse_hybrid_mode(Some(" dense ")),
            Ok(HybridMode::Dense)
        ));

        let err = parse_hybrid_mode(Some("dnese")).unwrap_err();
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(
            err.message.contains("dnese"),
            "error must name the bad value, got: {}",
            err.message
        );
        assert!(
            err.message.contains("sparse") && err.message.contains("dense"),
            "error must name the valid options, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn search_rejects_invalid_mode_as_invalid_params_not_a_silent_default() {
        // End-to-end through the actual tool handler (not just the parser): calling `search`
        // with a bad `mode` must surface as -32602 invalid_params naming the bad value, not
        // silently run an `rrf` search and return 200-style success.
        use rmcp::model::ErrorCode;

        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let err = mcp
            .search(Parameters(SearchParams {
                query: "widget".into(),
                limit: None,
                scope: None,
                mode: Some("dnese".into()),
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::INVALID_PARAMS,
            "got: {} {}",
            err.code.0,
            err.message
        );
        assert!(
            err.message.contains("dnese"),
            "error must name the bad value, got: {}",
            err.message
        );
    }

    #[test]
    fn mcp_err_surfaces_the_full_anyhow_cause_chain() {
        // `mcp_err` used to render only `e.to_string()` — an anyhow::Error's outermost context
        // frame — dropping every deeper `.context()` layer. Build a realistic multi-layer chain
        // the way `Store::open` does (create-dir context -> Connection::open context -> the
        // underlying io/rusqlite error) and confirm every layer survives into the MCP message,
        // not just the top one.
        let root = anyhow::anyhow!("unable to open database file: /no/such/dir/idx.db");
        let chained = root
            .context("opening index at /no/such/dir/idx.db")
            .context("initializing MCP store");

        let data = mcp_err(&chained);
        assert_eq!(data.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            data.message.contains("initializing MCP store"),
            "top frame missing: {}",
            data.message
        );
        assert!(
            data.message
                .contains("opening index at /no/such/dir/idx.db"),
            "middle frame missing: {}",
            data.message
        );
        assert!(
            data.message.contains("unable to open database file"),
            "root cause missing: {}",
            data.message
        );
    }

    #[tokio::test]
    async fn store_open_failure_surfaces_full_chain_through_a_real_tool_call() {
        // Same as above but end-to-end: point a real IndexaMcp at a db_path whose parent can
        // never be created (it's nested under a regular file, not a directory) so `Store::open`
        // fails with its real `.context()` chain, and confirm `get_stats` (which opens the
        // store via `mcp_err`) reports more than the bare top-level message.
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("not_a_dir");
        std::fs::write(&blocking_file, b"x").unwrap();
        let bogus_db_path = blocking_file.join("nested").join("idx.db");

        let mcp = IndexaMcp::new(
            bogus_db_path.clone(),
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let err = mcp.get_stats().await.unwrap_err();
        // The old `mcp_err` (bare `e.to_string()`) would have produced exactly this top context
        // frame and nothing else. Don't assert on the underlying io::Error's exact wording (it's
        // OS-dependent — "Not a directory" vs. Windows' phrasing differ); instead assert the
        // fixed chain-preservation property: the message starts with the top frame we control
        // AND carries strictly more detail after it, proving the deeper cause wasn't dropped.
        let top_frame_only = format!(
            "creating index directory {}",
            bogus_db_path.parent().unwrap().display()
        );
        assert!(
            err.message.starts_with(&top_frame_only),
            "top context frame missing: {}",
            err.message
        );
        assert!(
            err.message.len() > top_frame_only.len(),
            "expected root-cause detail appended after the top frame (chain must not be \
             truncated to just the top-level message), got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn ensure_ann_gates_on_config_threshold_and_caches() {
        // D3: MCP's ANN cache mirrors the web server's. Verify the gates + the build/cache path
        // hermetically (no Ollama — embeddings are written directly).
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let chunks: Vec<indexa_core::store::ChunkRecord> = (0..5)
                .map(|i| indexa_core::store::ChunkRecord {
                    entry_path: format!("/f{i}.rs"),
                    seq: 0,
                    heading: String::new(),
                    text: format!("chunk {i}"),
                    language: None,
                    embedding: Some(vec![i as f32; 8]),
                    embed_model: Some("t".to_owned()),
                    content_hash: None,
                })
                .collect();
            store.upsert_chunks(&chunks).unwrap();
        }
        let make = |cfg: Config| {
            IndexaMcp::new(
                dbpath.clone(),
                Arc::new(StubEmbedder),
                Arc::new(StubGenerator),
                Arc::new(cfg),
            )
        };

        // Default config: ANN on, but 5 chunks < ann_min_chunks (50k) → brute-force (None).
        assert!(
            make(Config::default()).ensure_ann().await.is_none(),
            "below ann_min_chunks → None"
        );

        // ANN explicitly off → None even with the threshold lowered.
        let mut off = Config::default();
        off.retrieval.ann = false;
        off.retrieval.ann_min_chunks = 1;
        assert!(make(off).ensure_ann().await.is_none(), "ann = false → None");

        // ANN on + threshold lowered → the index actually builds, and the second call reuses the
        // watermark-cached Arc (no rebuild).
        let mut on = Config::default();
        on.retrieval.ann_min_chunks = 1;
        let mcp = make(on);
        let first = mcp.ensure_ann().await.expect("builds above threshold");
        let second = mcp.ensure_ann().await.expect("cache hit");
        assert!(
            Arc::ptr_eq(&first, &second),
            "watermark-cached index is reused, not rebuilt"
        );
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
            // Mirror a real scan: the root directory is indexed as a Dir entry alongside the
            // file under it, so `root_paths()` reports the indexed root dir.
            store
                .upsert_entries(&[
                    Entry {
                        path: root.path().to_path_buf(),
                        kind: EntryKind::Dir,
                        size: 0,
                        modified: None,
                        hint: None,
                        is_binary: false,
                    },
                    Entry {
                        path: inside.clone(),
                        kind: EntryKind::File,
                        size: 11,
                        modified: None,
                        hint: None,
                        is_binary: false,
                    },
                ])
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
            .read_file_inner(inside.to_str().unwrap(), 0, "read_file")
            .is_ok());
        // A file outside every indexed root is rejected (the security contract).
        let err = mcp
            .read_file_inner(outside.to_str().unwrap(), 0, "read_file")
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("not within an indexed root"),
            "expected an indexed-root rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn read_file_offset_pages_a_later_window() {
        // A file inside an indexed root; reading from a byte offset returns a later slice
        // prefixed with a "bytes before" marker (the paging contract past the 40 KB cap).
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("inside.txt");
        std::fs::write(&inside, "0123456789abcdef").unwrap();

        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_entries(&[
                    Entry {
                        path: root.path().to_path_buf(),
                        kind: EntryKind::Dir,
                        size: 0,
                        modified: None,
                        hint: None,
                        is_binary: false,
                    },
                    Entry {
                        path: inside.clone(),
                        kind: EntryKind::File,
                        size: 16,
                        modified: None,
                        hint: None,
                        is_binary: false,
                    },
                ])
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

        let body = text_of(
            mcp.read_file_inner(inside.to_str().unwrap(), 10, "read_file")
                .unwrap(),
        );
        assert!(
            body.contains("…[10 bytes before]"),
            "offset read must note the bytes skipped, got: {body}"
        );
        assert!(
            body.contains("abcdef"),
            "offset read must include the later window, got: {body}"
        );
        assert!(
            !body.contains("0123456789"),
            "offset read must NOT include the skipped prefix, got: {body}"
        );
    }

    // ── FM-3: response caps + behavior fixes (revives #374 against current main) ──

    #[tokio::test]
    async fn dependencies_caps_each_group_and_says_so() {
        // A file with huge fan-out (150 imports) must not dump them all — capped at 100 with a
        // truthful "showing first 100" note; the header still reports the true total.
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let edges: Vec<indexa_core::store::EdgeRecord> = (0..150)
                .map(|i| indexa_core::store::EdgeRecord {
                    from_path: "/big.rs".into(),
                    kind: "imports".into(),
                    to_ref: format!("mod{i:03}"),
                })
                .collect();
            store.upsert_edges(&edges).unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .dependencies(Parameters(DependenciesParams {
                path: "/big.rs".into(),
                include_heritage: false,
            }))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.starts_with("Imports (150, showing first 100):"),
            "expected a truncated header, got first line: {:?}",
            text.lines().next().unwrap_or("")
        );
        assert_eq!(
            text.lines()
                .filter(|l| l.trim_start().starts_with('→'))
                .count(),
            100,
            "body must list exactly 100 items, got:\n{text}"
        );
    }

    #[tokio::test]
    async fn insights_duplicates_caps_clusters_and_says_so() {
        use indexa_core::store::SummaryRecord;
        // 51 exact-duplicate pairs (source_hash shared within each pair) — one more cluster
        // than the 50-cluster cap.
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            for i in 0..51 {
                for j in 0..2 {
                    store
                        .upsert_summary(&SummaryRecord {
                            path: format!("/dup{i}_{j}.rs"),
                            kind: "file".to_owned(),
                            parent_path: None,
                            depth: 0,
                            summary: "x".to_owned(),
                            summary_l0: None,
                            embedding: None,
                            child_count: 0,
                            byte_size: 10,
                            model: "t".to_owned(),
                            source_hash: format!("hash{i}"),
                            generated_at: 0,
                        })
                        .unwrap();
                }
            }
        }
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .insights_duplicates(Parameters(InsightsDuplicatesParams {
                threshold: None,
                exact: Some(true),
            }))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.starts_with("51 duplicate cluster(s), showing first 50:"),
            "expected a truncated header, got first line: {:?}",
            text.lines().next().unwrap_or("")
        );
        assert_eq!(
            text.matches("Cluster ").count(),
            50,
            "body must list exactly 50 clusters, got:\n{text}"
        );
    }

    #[tokio::test]
    async fn list_classifications_clamps_limit_to_at_least_one() {
        // Without the clamp, `limit: 0` maps to the store's "no limit" sentinel (LIMIT -1) and
        // would return all 3 seeded rows; `.clamp(1, 500)` turns it into exactly 1.
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_auto_classifications(&[
                    ("/a".to_owned(), "file".to_owned(), "work".to_owned(), 0.9),
                    ("/b".to_owned(), "file".to_owned(), "work".to_owned(), 0.8),
                    ("/c".to_owned(), "file".to_owned(), "work".to_owned(), 0.7),
                ])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .list_classifications(Parameters(ListClassificationsParams {
                source: None,
                limit: Some(0),
            }))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.starts_with("1 classification(s):"),
            "limit:0 must clamp up to (at least) 1, got: {text}"
        );
    }

    #[tokio::test]
    async fn list_files_by_category_clamps_limit_to_at_least_one() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_auto_classifications(&[
                    ("/a".to_owned(), "file".to_owned(), "work".to_owned(), 0.9),
                    ("/b".to_owned(), "file".to_owned(), "work".to_owned(), 0.8),
                    ("/c".to_owned(), "file".to_owned(), "work".to_owned(), 0.7),
                ])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .list_files_by_category(Parameters(ListFilesByCategoryParams {
                category: "work".into(),
                limit: Some(0),
            }))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.starts_with("1 file(s) classified as \"work\":"),
            "limit:0 must clamp up to (at least) 1, got: {text}"
        );
    }

    #[tokio::test]
    async fn get_chunk_context_clamps_radius_to_a_bounded_window() {
        // A huge `radius` must not dump the whole file — it's clamped to a bounded neighbor
        // window (2*25 + 1 = 51 chunks around the center seq), not all 100.
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let chunks: Vec<indexa_core::store::ChunkRecord> = (0..100)
                .map(|i| indexa_core::store::ChunkRecord {
                    entry_path: "/big.rs".into(),
                    seq: i,
                    heading: String::new(),
                    text: format!("chunk {i}"),
                    language: None,
                    embedding: None,
                    embed_model: None,
                    content_hash: None,
                })
                .collect();
            store.upsert_chunks(&chunks).unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .get_chunk_context(Parameters(GetChunkContextParams {
                path: "/big.rs".into(),
                seq: Some(50),
                radius: Some(9999),
            }))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.starts_with("51 chunk(s) from /big.rs"),
            "radius should clamp to a 51-chunk window; got first line: {:?}",
            text.lines().next().unwrap_or("")
        );
    }

    fn ask_params(
        session_id: Option<&str>,
        synthesize: Option<bool>,
        catalog: Option<bool>,
    ) -> AskParams {
        AskParams {
            question: "what does this do".to_owned(),
            scope: None,
            mode: None,
            agentic: None,
            rerank: None,
            rerank_backend: None,
            explain_savings: None,
            session_id: session_id.map(str::to_owned),
            top_k: None,
            synthesize,
            catalog,
        }
    }

    #[tokio::test]
    async fn ask_synthesize_false_wins_over_catalog_true_and_omits_the_session_footer() {
        // Sub-fixes 5+6: `synthesize:false` must win over `catalog:true` — the caller
        // explicitly asked for no synthesis, so it must get the richer retrieval-only slice,
        // not the catalog's file list. And because that turn is never persisted, the "pass the
        // same session_id to follow up" footer must not appear either — its promise must match
        // what actually happened.
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .ask(Parameters(ask_params(
                Some("conv1"),
                Some(false),
                Some(true),
            )))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.starts_with("RETRIEVED CONTEXT"),
            "synthesize:false must win over catalog:true — expected the retrieval-only slice \
             header, got: {text}"
        );
        assert!(
            !text.contains("Conversation: conv1"),
            "a retrieval-only (unrecorded) turn must not promise a session follow-up, got: {text}"
        );

        // Confirm the turn really wasn't persisted — the footer's absence must be honest, not
        // just cosmetic.
        let dbpath = dbdir.path().join("idx.db");
        let store = Store::open(&dbpath).unwrap();
        assert!(
            store.recent_turns("conv1", 10).unwrap().is_empty(),
            "retrieval-only ask must not record a conversation turn"
        );
    }

    #[tokio::test]
    async fn ask_synthesized_answer_records_the_turn_and_shows_the_session_footer() {
        // The positive case: when an answer IS synthesized (the default), the footer's promise
        // is honest — the turn really is recorded under session_id.
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .ask(Parameters(ask_params(Some("conv2"), None, None)))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.contains("Conversation: conv2"),
            "a synthesized answer must show the session-continuity footer, got: {text}"
        );

        let dbpath = dbdir.path().join("idx.db");
        let store = Store::open(&dbpath).unwrap();
        assert!(
            !store.recent_turns("conv2", 10).unwrap().is_empty(),
            "a synthesized answer's turn must actually be recorded (the footer's promise must \
             be true)"
        );
    }

    #[tokio::test]
    async fn search_dense_mode_surfaces_embedder_outage_instead_of_a_misleading_no_results() {
        // Sub-fix 7: in `dense` mode the embedding IS the search — a failed embed must be a
        // hard, explicit error, not a silent fallback that reports "No results" and hides an
        // embedder outage behind an indistinguishable empty-index message.
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        let _ = Store::open(&dbpath).unwrap();
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(FailingEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let err = mcp
            .search(Parameters(SearchParams {
                query: "widget".into(),
                limit: None,
                scope: None,
                mode: Some("dense".into()),
            }))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("embedder is unavailable"),
            "dense mode with a failing embedder must name the embedder outage, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn search_rrf_mode_falls_back_gracefully_when_the_embedder_fails() {
        // The graceful-fallback half of the same fix: `rrf` mode must NOT propagate the
        // embedder error — the sparse arm still works, so it returns its honest (here empty)
        // result instead of failing the whole call.
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        let _ = Store::open(&dbpath).unwrap();
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(FailingEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let res = mcp
            .search(Parameters(SearchParams {
                query: "widget".into(),
                limit: None,
                scope: None,
                mode: None, // default rrf
            }))
            .await;
        assert!(
            res.is_ok(),
            "rrf mode must gracefully fall back to sparse-only on an embedder failure, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn search_pack_records_usage_telemetry() {
        // Sub-fix 8: pack tool calls must record savings telemetry like every other
        // retrieval tool — this was missing entirely for `search_pack`/`export_pack`.
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let id = store.create_pack("proj", None).unwrap();
            store.add_pack_paths(&id, &["/root".to_owned()]).unwrap();
            store
                .upsert_chunks(&[indexa_core::store::ChunkRecord {
                    entry_path: "/root/a.rs".into(),
                    seq: 0,
                    heading: String::new(),
                    text: "unique_needle_xyz content".into(),
                    language: None,
                    embedding: None,
                    embed_model: None,
                    content_hash: None,
                }])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .search_pack(Parameters(SearchPackParams {
                name: "proj".into(),
                query: "unique_needle_xyz".into(),
                limit: None,
            }))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(
            text.contains("result(s) in pack"),
            "expected at least one hit, got: {text}"
        );

        let store = Store::open(&dbpath).unwrap();
        // `since_secs` is measured from "now" (`WHERE at >= unixepoch() - since_secs`), so 0
        // would race a second boundary between the insert and this read; use the same
        // week-wide window every other `usage_by_tool` caller uses.
        let usage = store
            .usage_by_tool(indexa_core::store::USAGE_WEEK_SECS)
            .unwrap();
        assert!(
            usage
                .iter()
                .any(|(tool, s)| tool == "search_pack" && s.calls > 0),
            "search_pack must record usage telemetry, got: {usage:?}"
        );
    }

    #[tokio::test]
    async fn export_pack_records_usage_telemetry() {
        use indexa_core::store::SummaryRecord;
        use indexa_core::walker::{Entry, EntryKind};
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let id = store.create_pack("proj", None).unwrap();
            store
                .add_pack_paths(&id, &["/root/a.rs".to_owned()])
                .unwrap();
            store
                .upsert_entries(&[Entry {
                    path: "/root/a.rs".into(),
                    kind: EntryKind::File,
                    size: 42,
                    modified: None,
                    hint: None,
                    is_binary: false,
                }])
                .unwrap();
            store
                .upsert_summary(&SummaryRecord {
                    path: "/root/a.rs".to_owned(),
                    kind: "file".to_owned(),
                    parent_path: None,
                    depth: 0,
                    summary: "A summary.".to_owned(),
                    summary_l0: None,
                    embedding: None,
                    child_count: 0,
                    byte_size: 42,
                    model: "t".to_owned(),
                    source_hash: "h1".to_owned(),
                    generated_at: 0,
                })
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let res = mcp
            .export_pack(Parameters(ExportPackParams {
                name: "proj".into(),
                format: None,
                depth: None,
                signatures: None,
                changed_since: None,
                category: None,
                include_graph: false,
                graph_format: None,
            }))
            .await
            .unwrap();
        let text = tool_text(res);
        assert!(!text.is_empty(), "export must produce content");

        let store = Store::open(&dbpath).unwrap();
        // `since_secs` is measured from "now" (`WHERE at >= unixepoch() - since_secs`), so 0
        // would race a second boundary between the insert and this read; use the same
        // week-wide window every other `usage_by_tool` caller uses.
        let usage = store
            .usage_by_tool(indexa_core::store::USAGE_WEEK_SECS)
            .unwrap();
        assert!(
            usage
                .iter()
                .any(|(tool, s)| tool == "export_pack" && s.calls > 0),
            "export_pack must record usage telemetry, got: {usage:?}"
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
             commit golden_tools.txt, and update the tool counts in README.md / AGENTS.md / USAGE.md / \
             docs/how-to/live-retrieval-over-mcp.md (doc_tool_count_matches_code enforces them)."
        );
    }

    /// 3.2 — every name in the `core` profile must be a real tool: a typo here would silently
    /// disable nothing for that name while still hiding it from the caller's expectations.
    #[test]
    fn core_profile_is_a_subset_of_the_full_tool_set() {
        let full: std::collections::HashSet<String> = IndexaMcp::tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        for name in CORE_TOOL_NAMES {
            assert!(
                full.contains(*name),
                "CORE_TOOL_NAMES has '{name}', which is not a real tool — typo?"
            );
        }
    }

    /// Wave 7 bug 2 — `get_info()`'s `core`-profile instructions must never name a tool the
    /// `core` profile doesn't actually expose (a caller told about `read_file`/`code_graph`/etc.
    /// would call it and get rejected by `active_tool_router`). Every backtick-quoted plain
    /// snake_case identifier in the prose (resource URIs and prompt names contain `:`/`/`/`-`
    /// and are skipped) must be a real `CORE_TOOL_NAMES` entry.
    #[test]
    fn core_instructions_never_names_a_non_core_tool() {
        let text = core_instructions();
        let parts: Vec<&str> = text.split('`').collect();
        let mut distinct_tool_terms: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for (i, part) in parts.iter().enumerate() {
            // `split('`')` alternates plain-text / backtick-quoted spans — odd indices are
            // the backtick-quoted ones.
            if i % 2 != 1 {
                continue;
            }
            let is_snake_case_identifier =
                !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c == '_');
            if !is_snake_case_identifier {
                continue; // a resource URI, prompt name, or "synthesize: false" — not a tool name
            }
            assert!(
                CORE_TOOL_NAMES.contains(part),
                "core_instructions() names `{part}`, which is NOT in CORE_TOOL_NAMES — a \
                 core-profile caller would be told about a tool it can't actually call"
            );
            distinct_tool_terms.insert(part);
        }
        // Sanity on the DISTINCT set (not raw mention count — `ask` alone appears 3 times, so a
        // naive tally could reach `CORE_TOOL_NAMES.len()` even if several real core tools were
        // silently dropped from the prose while `ask`/`search` kept being repeated).
        for name in CORE_TOOL_NAMES {
            assert!(
                distinct_tool_terms.contains(name),
                "core_instructions() never mentions core tool `{name}` even once"
            );
        }
    }

    /// Wave 7 bug 2 — `get_info()` actually wires `core_instructions()`/`FULL_INSTRUCTIONS`
    /// through to the live `ToolProfile`, not just as dead helper functions.
    #[test]
    fn get_info_instructions_match_the_live_tool_profile() {
        let dbdir = tempfile::tempdir().unwrap();
        let full = mcp_with_db(&dbdir);
        assert_eq!(
            full.get_info().instructions.as_deref(),
            Some(FULL_INSTRUCTIONS),
            "full profile must be byte-identical to pre-3.2 instructions"
        );

        let dbdir2 = tempfile::tempdir().unwrap();
        let dbpath = dbdir2.path().join("idx.db");
        let _ = Store::open(&dbpath).unwrap();
        let core = IndexaMcp::new_with_profile(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
            ToolProfile::Core,
        );
        assert_eq!(
            core.get_info().instructions,
            Some(core_instructions()),
            "core profile must serve the core-specific instructions"
        );
    }

    /// 3.2 — `core` profile: `list_tools` advertises only the core subset, and calling a
    /// non-core tool is rejected (not merely unlisted) while a core tool still works.
    #[tokio::test]
    async fn tool_profile_core_hides_and_blocks_non_core_tools() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        let _ = Store::open(&dbpath).unwrap();
        let mcp = IndexaMcp::new_with_profile(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
            ToolProfile::Core,
        );

        let router = mcp.active_tool_router();
        let listed: std::collections::HashSet<String> = router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            listed,
            CORE_TOOL_NAMES.iter().map(|s| s.to_string()).collect(),
            "core profile must advertise exactly the core set"
        );

        // A non-core tool (e.g. `code_graph`) is rejected outright, not just unlisted.
        assert!(
            router.get("code_graph").is_none(),
            "a disabled tool must not resolve via get() either"
        );

        // A core tool still works end-to-end through the profile-filtered router.
        let text = mcp.list_packs().await.unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!body.is_empty(), "list_packs must still respond directly");
    }

    /// 3.2 — the default (unset) profile is `Full` and must be byte-identical to pre-3.2
    /// behavior: every tool listed, none disabled.
    #[test]
    fn tool_profile_default_is_full_and_unfiltered() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        assert_eq!(mcp.tool_profile, ToolProfile::Full);
        let router = mcp.active_tool_router();
        assert_eq!(
            router.list_all().len(),
            IndexaMcp::tool_router().list_all().len()
        );
    }

    /// `ToolProfile::parse` fails open: only the exact string `"core"` narrows the surface,
    /// everything else (empty, unset, typo'd) falls back to `Full` — a misconfigured profile
    /// must never accidentally hide tools an agent needs.
    #[test]
    fn tool_profile_parse_fails_open_to_full() {
        assert_eq!(ToolProfile::parse("core"), ToolProfile::Core);
        assert_eq!(ToolProfile::parse("full"), ToolProfile::Full);
        assert_eq!(ToolProfile::parse(""), ToolProfile::Full);
        assert_eq!(ToolProfile::parse("Core"), ToolProfile::Full);
        assert_eq!(ToolProfile::parse("bogus"), ToolProfile::Full);
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
            "AGENTS.md",
            "USAGE.md",
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
                cycles: false,
                modules: false,
            }))
            .await
            .unwrap(),
        );
        assert!(
            caveated.contains("No call edges") || caveated.contains("bare-name"),
            "code_graph must either report emptiness or carry the bare-name caveat, got: {caveated}"
        );
    }

    /// 2.4 — `trace_path` end-to-end: a scoped call chain resolves through one hop and
    /// the response names every file in the chain plus its resolving tier.
    #[tokio::test]
    async fn trace_path_reports_the_resolved_hop_chain() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_edges(&[
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/handler.rs".into(),
                        kind: "calls".into(),
                        to_ref: "helper".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/db.rs".into(),
                        kind: "defines".into(),
                        to_ref: "helper".into(),
                    },
                ])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let text = mcp
            .trace_path(Parameters(TracePathParams {
                from: "/proj/handler.rs".into(),
                to: "/proj/db.rs".into(),
                max_depth: None,
            }))
            .await
            .unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("/proj/handler.rs"), "got: {body}");
        assert!(body.contains("/proj/db.rs"), "got: {body}");
        assert!(body.contains("1 hop"), "got: {body}");
    }

    /// 2.4 — no confirmed path reports the specific "not found" phrasing, not an error.
    #[tokio::test]
    async fn trace_path_reports_no_path_when_unreachable() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let text = mcp
            .trace_path(Parameters(TracePathParams {
                from: "/proj/a.rs".into(),
                to: "/proj/b.rs".into(),
                max_depth: None,
            }))
            .await
            .unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("No confirmed path"), "got: {body}");
    }

    /// `dependency_closure` — default direction ("callee") walks what the seed transitively
    /// calls, across multiple hops, resolved and formatted end-to-end through the real tool
    /// handler (not just the core function).
    #[tokio::test]
    async fn dependency_closure_walks_callee_direction_by_default() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_edges(&[
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/a.rs".into(),
                        kind: "calls".into(),
                        to_ref: "helper".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/b.rs".into(),
                        kind: "defines".into(),
                        to_ref: "helper".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/b.rs".into(),
                        kind: "calls".into(),
                        to_ref: "inner".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/c.rs".into(),
                        kind: "defines".into(),
                        to_ref: "inner".into(),
                    },
                ])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let text = mcp
            .dependency_closure(Parameters(DependencyClosureParams {
                target: "/proj/a.rs".into(),
                direction: None,
                depth: Some(2),
                strict: false,
            }))
            .await
            .unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("callee closure"), "got: {body}");
        assert!(body.contains("/proj/b.rs"), "got: {body}");
        assert!(body.contains("/proj/c.rs"), "got: {body}");
    }

    /// `dependency_closure` — `direction: "caller"` walks the reverse edge (who transitively
    /// depends on the target), from the same fixture shape as the callee test above.
    #[tokio::test]
    async fn dependency_closure_walks_caller_direction_when_requested() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_edges(&[
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/a.rs".into(),
                        kind: "calls".into(),
                        to_ref: "helper".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/b.rs".into(),
                        kind: "defines".into(),
                        to_ref: "helper".into(),
                    },
                ])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let text = mcp
            .dependency_closure(Parameters(DependencyClosureParams {
                target: "/proj/b.rs".into(),
                direction: Some("caller".into()),
                depth: Some(1),
                strict: false,
            }))
            .await
            .unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("caller closure"), "got: {body}");
        assert!(body.contains("/proj/a.rs"), "got: {body}");
    }

    /// `dependency_closure` — an unrecognized `direction` is a caller error (-32602), not a
    /// silent fallback, matching `parse_hybrid_mode`'s convention for other enum-like params.
    #[tokio::test]
    async fn dependency_closure_rejects_invalid_direction_as_invalid_params() {
        use rmcp::model::ErrorCode;

        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let err = mcp
            .dependency_closure(Parameters(DependencyClosureParams {
                target: "/proj/a.rs".into(),
                direction: Some("sideways".into()),
                depth: None,
                strict: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "got: {}", err.message);
        assert!(err.message.contains("sideways"), "got: {}", err.message);
    }

    /// `dependency_closure` — a target with no indexed edges reports the specific
    /// "not found" phrasing instead of an empty success or an error.
    #[tokio::test]
    async fn dependency_closure_reports_unknown_target() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let text = mcp
            .dependency_closure(Parameters(DependencyClosureParams {
                target: "/proj/nowhere.rs".into(),
                direction: None,
                depth: None,
                strict: false,
            }))
            .await
            .unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("No indexed file or symbol found"),
            "got: {body}"
        );
    }

    /// 2.5 — `symbol_context` end-to-end: definitions (kind + line range from the
    /// symbols table), callers, and an anchored note (2.6) all appear in one response.
    #[tokio::test]
    async fn symbol_context_reports_definition_callers_and_anchored_notes() {
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_edges(&[
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/lib.rs".into(),
                        kind: "defines".into(),
                        to_ref: "run".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/main.rs".into(),
                        kind: "calls".into(),
                        to_ref: "run".into(),
                    },
                ])
                .unwrap();
            store
                .upsert_symbols(&[indexa_core::store::SymbolRecord {
                    path: "/proj/lib.rs".into(),
                    name: "run".into(),
                    kind: "fn".into(),
                    start_line: 10,
                    end_line: 20,
                }])
                .unwrap();
            store
                .upsert_note_anchor(
                    "/notes/run-gotcha.md",
                    "run",
                    "symbol",
                    "Watch the retry loop",
                    "eng",
                )
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let text = mcp
            .symbol_context(Parameters(SymbolContextParams {
                symbol: "run".into(),
                limit: None,
            }))
            .await
            .unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("/proj/lib.rs:10-20 (fn)"), "got: {body}");
        assert!(body.contains("/proj/main.rs"), "got: {body}");
        assert!(body.contains("Watch the retry loop"), "got: {body}");
    }

    /// 2.3 — `changed_impact` end-to-end against a REAL git repo: an unstaged edit to a
    /// symbol's line range maps through the actual `git diff` subprocess to the symbol,
    /// then to its caller via `blast_radius`.
    #[cfg(unix)]
    #[tokio::test]
    async fn changed_impact_maps_a_real_git_diff_to_the_touched_symbol_and_its_caller() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("lib.rs"), "fn target_fn() {\n    1\n}\n").unwrap();
        run(&["add", "lib.rs"]);
        run(&["commit", "-q", "-m", "init"]);
        // Edit within target_fn's line range (1-3), unstaged.
        std::fs::write(root.join("lib.rs"), "fn target_fn() {\n    2\n}\n").unwrap();

        let lib_path = root.join("lib.rs").to_string_lossy().into_owned();
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_entries(&[indexa_core::walker::Entry {
                    path: root.to_path_buf(),
                    kind: indexa_core::walker::EntryKind::Dir,
                    size: 0,
                    modified: None,
                    hint: None,
                    is_binary: false,
                }])
                .unwrap();
            store
                .upsert_symbols(&[indexa_core::store::SymbolRecord {
                    path: lib_path.clone(),
                    name: "target_fn".into(),
                    kind: "fn".into(),
                    start_line: 1,
                    end_line: 3,
                }])
                .unwrap();
            store
                .upsert_edges(&[
                    indexa_core::store::EdgeRecord {
                        from_path: lib_path.clone(),
                        kind: "defines".into(),
                        to_ref: "target_fn".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/caller.rs".into(),
                        kind: "calls".into(),
                        to_ref: "target_fn".into(),
                    },
                ])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let text = mcp
            .changed_impact(Parameters(ChangedImpactParams {
                root: Some(root.to_string_lossy().into_owned()),
                scope: None,
                strict: false,
                depth: None,
                include_heritage: false,
            }))
            .await
            .unwrap();
        let body = text
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("target_fn"), "got: {body}");
        assert!(body.contains("/proj/caller.rs"), "got: {body}");
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

    /// Mirrors `dummy_chunk_embedded` in indexa-core's own store tests — embedded (so it counts
    /// toward `chunks_current_for_mtime`) and non-null `language` (so `code_chunks_under`, the
    /// `--signatures` export path, picks it up without needing a summary fixture).
    fn dummy_chunk_embedded(path: &str, text: &str) -> indexa_core::store::ChunkRecord {
        indexa_core::store::ChunkRecord {
            entry_path: path.to_owned(),
            seq: 0,
            heading: String::new(),
            text: text.to_owned(),
            language: Some("rust".to_owned()),
            embedding: Some(vec![0.1, 0.2, 0.3]),
            embed_model: Some("test".to_owned()),
            content_hash: None,
        }
    }

    #[tokio::test]
    async fn export_pack_reports_stale_files_count_in_header() {
        // G2b: export_pack's XML header must carry the live stale-member count (same check
        // `pack show`/CLI+web export use), not just whatever was true when the pack was built.
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let dbpath = dbdir.path().join("idx.db");

        let file = dbdir.path().join("a.rs");
        std::fs::write(&file, b"fn foo() {}").unwrap();
        let file_s = file.to_string_lossy().to_string();

        let mut store = Store::open(&dbpath).unwrap();
        store
            .upsert_chunks(&[dummy_chunk_embedded(&file_s, "fn foo() {}")])
            .unwrap();
        // Pin indexed_at to the epoch — long before the file's real mtime — so it reads stale.
        store
            .db_connection()
            .execute(
                "UPDATE chunks SET indexed_at = 1 WHERE entry_path = ?1",
                rusqlite::params![file_s],
            )
            .unwrap();
        let pack_id = store.create_pack("code", None).unwrap();
        store
            .add_pack_paths(&pack_id, std::slice::from_ref(&file_s))
            .unwrap();
        drop(store);

        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let res = mcp
            .export_pack(Parameters(ExportPackParams {
                name: "code".into(),
                format: None,
                depth: None,
                signatures: Some(true),
                changed_since: None,
                category: None,
                include_graph: false,
                graph_format: None,
            }))
            .await;
        assert!(res.is_ok(), "export should succeed: {res:?}");
        let body = text_of(res.unwrap());
        assert!(
            body.contains("stale_files=\"1\""),
            "expected stale_files=\"1\" in the export header, got: {body}"
        );
    }

    #[tokio::test]
    async fn export_pack_with_include_graph_appends_mermaid_section() {
        // 1.6: export_pack's include_graph/graph_format params must append a fenced
        // ```mermaid flowchart over the pack's call graph when graph_format is "mermaid".
        use indexa_core::store::SummaryRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_summary(&SummaryRecord {
                    path: "/proj".into(),
                    kind: "dir".into(),
                    parent_path: None,
                    depth: 0,
                    summary: "A small project.".into(),
                    summary_l0: None,
                    embedding: None,
                    child_count: 0,
                    byte_size: 0,
                    model: "test".into(),
                    source_hash: String::new(),
                    generated_at: 0,
                })
                .unwrap();
            store
                .upsert_edges(&[
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/a.rs".into(),
                        kind: "calls".into(),
                        to_ref: "run".into(),
                    },
                    indexa_core::store::EdgeRecord {
                        from_path: "/proj/b.rs".into(),
                        kind: "defines".into(),
                        to_ref: "run".into(),
                    },
                ])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        mcp.create_pack(Parameters(CreatePackMcpParams {
            name: "proj-pack".into(),
            description: None,
        }))
        .await
        .unwrap();
        mcp.add_pack_paths(Parameters(PackPathsParams {
            name: "proj-pack".into(),
            paths: vec!["/proj".into()],
        }))
        .await
        .unwrap();

        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Default (include_graph: false) — no mermaid section.
        let flat = text_of(
            mcp.export_pack(Parameters(ExportPackParams {
                name: "proj-pack".into(),
                format: Some("md".into()),
                depth: None,
                signatures: None,
                changed_since: None,
                category: None,
                include_graph: false,
                graph_format: None,
            }))
            .await
            .unwrap(),
        );
        assert!(!flat.contains("```mermaid"));

        // include_graph + graph_format: mermaid — the fenced block appears.
        let with_graph = text_of(
            mcp.export_pack(Parameters(ExportPackParams {
                name: "proj-pack".into(),
                format: Some("md".into()),
                depth: None,
                signatures: None,
                changed_since: None,
                category: None,
                include_graph: true,
                graph_format: Some("mermaid".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(with_graph.contains("```mermaid"));
        assert!(with_graph.contains("flowchart TD"));
    }

    /// 4.2 — `export_pack` with `format: "okf"` returns the OKF bundle concatenated with
    /// `--- file: <path> ---` separators (a real directory bundle is the CLI's job).
    #[tokio::test]
    async fn export_pack_okf_format_returns_a_concatenated_bundle() {
        use indexa_core::store::SummaryRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_summary(&SummaryRecord {
                    path: "/proj".into(),
                    kind: "dir".into(),
                    parent_path: None,
                    depth: 0,
                    summary: "A small project.".into(),
                    summary_l0: Some("A small project abstract.".into()),
                    embedding: None,
                    child_count: 0,
                    byte_size: 0,
                    model: "test".into(),
                    source_hash: "deadbeef00".into(),
                    generated_at: 0,
                })
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        mcp.create_pack(Parameters(CreatePackMcpParams {
            name: "proj-pack".into(),
            description: None,
        }))
        .await
        .unwrap();
        mcp.add_pack_paths(Parameters(PackPathsParams {
            name: "proj-pack".into(),
            paths: vec!["/proj".into()],
        }))
        .await
        .unwrap();

        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let body = text_of(
            mcp.export_pack(Parameters(ExportPackParams {
                name: "proj-pack".into(),
                format: Some("okf".into()),
                depth: None,
                signatures: None,
                changed_since: None,
                category: None,
                include_graph: false,
                graph_format: None,
            }))
            .await
            .unwrap(),
        );
        assert!(body.contains("--- file: index.md ---"));
        assert!(body.contains("--- file: log.md ---"));
        assert!(body.contains("okf_version"));
        assert!(body.contains("A small project abstract."));
        assert!(
            !body.contains('<'),
            "OKF bundle must be pure Markdown, never HTML"
        );
    }

    #[tokio::test]
    async fn search_flags_a_stale_result_and_summarizes_in_footer() {
        // 1.2: a cited file whose on-disk mtime is newer than what's indexed gets a "(stale)"
        // marker inline and a footer summary; a fresh file gets neither.
        use indexa_core::store::ChunkRecord;
        let root = tempfile::tempdir().unwrap();
        let fresh = root.path().join("fresh.rs");
        let stale = root.path().join("stale.rs");
        std::fs::write(&fresh, "fn widget_alpha() {}").unwrap();
        std::fs::write(&stale, "fn widget_beta() {}").unwrap();
        let fresh_path = fresh.to_str().unwrap().to_owned();
        let stale_path = stale.to_str().unwrap().to_owned();

        let dbdir = tempfile::tempdir().unwrap();
        {
            let mut store = Store::open(&dbdir.path().join("idx.db")).unwrap();
            let chunk = |path: &str| ChunkRecord {
                entry_path: path.to_owned(),
                seq: 0,
                heading: String::new(),
                text: "a widget function definition".to_owned(),
                language: None,
                // Embedded — chunks_current_for_mtime also requires every chunk to carry an
                // embedding; without one both rows would read "stale" regardless of indexed_at,
                // masking the mtime comparison this test targets.
                embedding: Some(vec![0.1, 0.2, 0.3]),
                embed_model: Some("test".to_owned()),
                content_hash: None,
            };
            store
                .upsert_chunks(&[chunk(&fresh_path), chunk(&stale_path)])
                .unwrap();
            // Fresh: indexed far in the future relative to disk mtime. Stale: indexed long ago.
            store
                .db_connection()
                .execute(
                    "UPDATE chunks SET indexed_at = ?1 WHERE entry_path = ?2",
                    rusqlite::params![i64::MAX / 2, fresh_path],
                )
                .unwrap();
            store
                .db_connection()
                .execute(
                    "UPDATE chunks SET indexed_at = ?1 WHERE entry_path = ?2",
                    rusqlite::params![1_i64, stale_path],
                )
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let out = text_of(
            mcp.search(Parameters(SearchParams {
                query: "widget".into(),
                limit: None,
                scope: None,
                mode: Some("sparse".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains(&format!("{stale_path} (stale)")),
            "stale result must be marked inline: {out}"
        );
        assert!(
            !out.contains(&format!("{fresh_path} (stale)")),
            "fresh result must not be marked stale: {out}"
        );
        assert!(
            out.contains("1 of 2 result file(s) changed on disk since indexing"),
            "footer summary missing: {out}"
        );
    }

    #[tokio::test]
    async fn search_honors_ext_predicate_when_enabled() {
        // 1.8: with query_predicates on, "ext:rs" restricts hits to .rs files and is stripped
        // from the searched text.
        use indexa_core::store::ChunkRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let chunk = |path: &str| ChunkRecord {
                entry_path: path.to_owned(),
                seq: 0,
                heading: String::new(),
                text: "a widget function definition".to_owned(),
                language: None,
                embedding: None,
                embed_model: None,
                content_hash: None,
            };
            store
                .upsert_chunks(&[chunk("/proj/widget.rs"), chunk("/proj/widget.md")])
                .unwrap();
        }
        let mut cfg = Config::default();
        cfg.retrieval.query_predicates = true;
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(cfg),
        );
        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let out = text_of(
            mcp.search(Parameters(SearchParams {
                query: "ext:rs widget".into(),
                limit: None,
                scope: None,
                mode: Some("sparse".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(out.contains("/proj/widget.rs"), "{out}");
        assert!(
            !out.contains("/proj/widget.md"),
            "ext:rs must exclude the .md file: {out}"
        );
    }

    #[tokio::test]
    async fn search_honors_type_predicate_when_enabled() {
        // Named file-type sets: "type:python" restricts hits to .py/.pyi files (both members
        // of the set) while excluding a .rs file, and is stripped from the searched text.
        use indexa_core::store::ChunkRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let chunk = |path: &str| ChunkRecord {
                entry_path: path.to_owned(),
                seq: 0,
                heading: String::new(),
                text: "an auth handler definition".to_owned(),
                language: None,
                embedding: None,
                embed_model: None,
                content_hash: None,
            };
            store
                .upsert_chunks(&[
                    chunk("/proj/auth.py"),
                    chunk("/proj/auth.pyi"),
                    chunk("/proj/auth.rs"),
                ])
                .unwrap();
        }
        let mut cfg = Config::default();
        cfg.retrieval.query_predicates = true;
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(cfg),
        );
        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let out = text_of(
            mcp.search(Parameters(SearchParams {
                query: "type:python auth".into(),
                limit: None,
                scope: None,
                mode: Some("sparse".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(out.contains("/proj/auth.py"), "{out}");
        assert!(out.contains("/proj/auth.pyi"), "{out}");
        assert!(
            !out.contains("/proj/auth.rs"),
            "type:python must exclude the .rs file: {out}"
        );
    }

    #[tokio::test]
    async fn search_predicates_are_a_noop_when_disabled() {
        // The default (query_predicates: false) must searchd the literal query text,
        // predicates and all — behavior-neutral for anyone not opting in.
        use indexa_core::store::ChunkRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_chunks(&[ChunkRecord {
                    entry_path: "/proj/widget.rs".into(),
                    seq: 0,
                    heading: String::new(),
                    text: "ext:rs widget marker text".to_owned(),
                    language: None,
                    embedding: None,
                    embed_model: None,
                    content_hash: None,
                }])
                .unwrap();
        }
        let mcp = mcp_with_db(&dbdir);
        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Searching the literal token "ext:rs" must still hit the chunk whose FTS-indexed
        // text contains it verbatim, proving the predicate grammar never engaged.
        let out = text_of(
            mcp.search(Parameters(SearchParams {
                query: "ext:rs".into(),
                limit: None,
                scope: None,
                mode: Some("sparse".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(out.contains("/proj/widget.rs"), "{out}");
    }

    /// Wave 7 bug 3a — the `ext:`/`type:` filter is applied AFTER `hybrid_search_with_ann`
    /// already truncated to the requested `limit`, so a real match ranked below that naive
    /// cutoff was silently invisible ("no results" when a match genuinely exists further down).
    /// Four short, tightly-matching `.md` chunks outrank one long, diluted `.rs` chunk that
    /// also matches "widget" (BM25 length-normalizes, so the long document ranks worse) —
    /// with `limit: 1`, a naive fetch-then-filter would already have thrown the `.rs` hit away
    /// before the filter ever sees it.
    #[tokio::test]
    async fn ext_predicate_finds_a_match_below_the_naive_limit_then_filter_cutoff() {
        use indexa_core::store::ChunkRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let short = |path: &str| ChunkRecord {
                entry_path: path.to_owned(),
                seq: 0,
                heading: String::new(),
                text: "widget".to_owned(),
                language: None,
                embedding: None,
                embed_model: None,
                content_hash: None,
            };
            // BM25 length-normalizes: a "widget" mention diluted across a long document
            // ranks below four short, single-word "widget" chunks.
            let long_text = format!("{}widget {}", "filler ".repeat(200), "filler ".repeat(200));
            store
                .upsert_chunks(&[
                    short("/proj/a.md"),
                    short("/proj/b.md"),
                    short("/proj/c.md"),
                    short("/proj/d.md"),
                    ChunkRecord {
                        entry_path: "/proj/impl.rs".to_owned(),
                        seq: 0,
                        heading: String::new(),
                        text: long_text,
                        language: None,
                        embedding: None,
                        embed_model: None,
                        content_hash: None,
                    },
                ])
                .unwrap();
        }
        let mut cfg = Config::default();
        cfg.retrieval.query_predicates = true;
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(cfg),
        );
        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let out = text_of(
            mcp.search(Parameters(SearchParams {
                query: "ext:rs widget".into(),
                limit: Some(1),
                scope: None,
                mode: Some("sparse".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out.contains("/proj/impl.rs"),
            "a genuine .rs match ranked below the naive limit=1 cutoff must still surface: {out}"
        );
    }

    /// Wave 7 bug 3b — `ext:` matching must be case-insensitive in both directions: an
    /// uppercase predicate value must match a lowercase stored extension, and vice versa.
    #[tokio::test]
    async fn ext_predicate_matches_case_insensitively_both_directions() {
        use indexa_core::store::ChunkRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            let chunk = |path: &str| ChunkRecord {
                entry_path: path.to_owned(),
                seq: 0,
                heading: String::new(),
                text: "a widget function definition".to_owned(),
                language: None,
                embedding: None,
                embed_model: None,
                content_hash: None,
            };
            store
                .upsert_chunks(&[chunk("/proj/lower.rs"), chunk("/proj/UPPER.RS")])
                .unwrap();
        }
        let mut cfg = Config::default();
        cfg.retrieval.query_predicates = true;
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(cfg),
        );
        let text_of = |r: CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Direction 1: an UPPERCASE predicate value must match a lowercase-extension file.
        let out_upper_predicate = text_of(
            mcp.search(Parameters(SearchParams {
                query: "ext:RS widget".into(),
                limit: None,
                scope: None,
                mode: Some("sparse".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out_upper_predicate.contains("/proj/lower.rs"),
            "ext:RS must match a stored .rs file: {out_upper_predicate}"
        );
        // Direction 2: a lowercase predicate value must match an UPPERCASE-extension file.
        let out_lower_predicate = text_of(
            mcp.search(Parameters(SearchParams {
                query: "ext:rs widget".into(),
                limit: None,
                scope: None,
                mode: Some("sparse".into()),
            }))
            .await
            .unwrap(),
        );
        assert!(
            out_lower_predicate.contains("/proj/UPPER.RS"),
            "ext:rs must match a stored .RS file: {out_lower_predicate}"
        );
    }

    #[tokio::test]
    async fn ask_with_session_id_records_a_conversation() {
        // Conversational Ask over MCP: two `ask` calls with the same session_id persist two
        // turns the next call can see (even over an empty index, which returns a graceful
        // no-match answer). Omitting the id records nothing.
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let dbpath = dbdir.path().join("idx.db");

        let mk = |q: &str, sid: Option<&str>| AskParams {
            question: q.to_owned(),
            scope: None,
            mode: None,
            agentic: Some(false),
            rerank: Some(false),
            rerank_backend: None,
            explain_savings: None,
            session_id: sid.map(str::to_owned),
            top_k: None,
            synthesize: None,
            catalog: None,
        };

        assert!(mcp
            .ask(Parameters(mk("what is here?", Some("c1"))))
            .await
            .is_ok());
        assert!(mcp
            .ask(Parameters(mk("and what else?", Some("c1"))))
            .await
            .is_ok());
        // A stateless ask must not create a session row.
        assert!(mcp.ask(Parameters(mk("stateless?", None))).await.is_ok());

        let store = Store::open(&dbpath).unwrap();
        let turns = store.turns_for_session("c1").unwrap();
        assert_eq!(turns.len(), 2, "both session turns persisted");
        assert_eq!(turns[0].question, "what is here?");
        assert_eq!(turns[1].question, "and what else?");
    }

    // ── Resources + Prompts (separate protocol surfaces; tool count unaffected) ──

    /// Golden prompt list: any added/removed/renamed prompt is a deliberate diff of
    /// `golden_prompts.txt`. Regenerate with `INDEXA_UPDATE_GOLDEN=1 cargo test -p indexa-mcp`.
    #[test]
    fn prompt_contract_golden_list() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        let mut names: Vec<String> = mcp
            .list_prompts_inner()
            .iter()
            .map(|p| p.name.to_string())
            .collect();
        names.sort();
        let actual = names.join("\n") + "\n";
        let golden_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden_prompts.txt");
        if std::env::var("INDEXA_UPDATE_GOLDEN").is_ok() {
            std::fs::write(&golden_path, &actual).unwrap();
            return;
        }
        let golden = std::fs::read_to_string(&golden_path)
            .expect("crates/mcp/golden_prompts.txt missing — INDEXA_UPDATE_GOLDEN=1 cargo test")
            .replace("\r\n", "\n");
        assert_eq!(
            actual, golden,
            "MCP prompt surface changed; update golden_prompts.txt"
        );
    }

    #[test]
    fn every_prompt_has_a_description() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        for p in mcp.list_prompts_inner() {
            assert!(
                !p.description.as_deref().unwrap_or("").trim().is_empty(),
                "prompt '{}' has no description",
                p.name
            );
        }
    }

    #[test]
    fn resources_round_trip() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        // Static list + templates are non-empty and stable.
        assert!(!mcp.list_resources_inner().is_empty());
        assert!(!mcp.resource_templates_inner().is_empty());
        // Known URIs resolve (overview is graceful even on an empty index).
        assert!(mcp.read_resource_inner("indexa://overview").is_ok());
        assert!(mcp.read_resource_inner("indexa://packs").is_ok());
        // Unknown / unsupported URIs error rather than panic.
        assert!(mcp.read_resource_inner("indexa://nope").is_err());
        assert!(mcp.read_resource_inner("file:///etc/passwd").is_err());
    }

    #[test]
    fn prompts_round_trip_and_validate_args() {
        let dbdir = tempfile::tempdir().unwrap();
        let mcp = mcp_with_db(&dbdir);
        // No-arg prompt always resolves.
        assert!(mcp.get_prompt_inner("onboarding-overview", None).is_ok());
        // Required arg missing → invalid_params; present → ok.
        assert!(mcp.get_prompt_inner("explain-file", None).is_err());
        let mut args = serde_json::Map::new();
        args.insert("path".into(), serde_json::json!("/proj/x.rs"));
        assert!(mcp.get_prompt_inner("explain-file", Some(&args)).is_ok());
        // Unknown prompt → error.
        assert!(mcp.get_prompt_inner("does-not-exist", None).is_err());
    }

    #[test]
    fn summary_resource_redacts_secrets() {
        use indexa_core::store::SummaryRecord;
        let dbdir = tempfile::tempdir().unwrap();
        let dbpath = dbdir.path().join("idx.db");
        {
            let mut store = Store::open(&dbpath).unwrap();
            store
                .upsert_summary(&SummaryRecord {
                    path: "/proj/secrets.txt".into(),
                    kind: "file".into(),
                    parent_path: Some("/proj".into()),
                    depth: 1,
                    // A canonical AWS test key — redact_secrets must strip it from the resource.
                    summary: "Config note: aws_key = AKIAIOSFODNN7EXAMPLE in the deploy script."
                        .into(),
                    summary_l0: None,
                    embedding: None,
                    child_count: 0,
                    byte_size: 50,
                    model: "test".into(),
                    source_hash: String::new(),
                    generated_at: 0,
                })
                .unwrap();
        }
        let mcp = IndexaMcp::new(
            dbpath,
            Arc::new(StubEmbedder),
            Arc::new(StubGenerator),
            Arc::new(Config::default()),
        );
        let res = mcp
            .read_resource_inner("indexa://summary//proj/secrets.txt")
            .unwrap();
        let json = serde_json::to_string(&res).unwrap();
        assert!(
            !json.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key leaked through resource"
        );
        assert!(
            json.contains("[REDACTED-aws-key]"),
            "expected redaction marker"
        );
    }
}
