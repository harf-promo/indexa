//! Code-graph tools: `dependencies`, `who_imports`, `who_calls`,
//! `blast_radius`, `code_graph`, and `related_files`.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use crate::{mcp_err, ok_text, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DependenciesParams {
    /// Absolute path of an indexed code file.
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WhoImportsParams {
    /// Module/import path to find importers of, exactly as written in source
    /// (e.g. `std::fs`, `os`, `./util`).
    pub module: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WhoCallsParams {
    /// Bare function or method name to find callers of (e.g. `parse`, `render`, `connect`).
    pub symbol: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlastRadiusParams {
    /// Bare function or method name whose blast radius to compute.
    pub symbol: String,
    /// Strict: on the transitive hop, follow only symbols defined in exactly one file
    /// (fewer false positives from common names). Default false (broader match).
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RelatedFilesParams {
    /// Absolute path of the file to find related files for.
    pub path: String,
    /// Max related files to return (default 15).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeGraphParams {
    /// Absolute path prefix to scope the graph to (e.g. a repo or crate directory).
    pub scope: String,
    /// Max edges to return, heaviest first (default 200).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Strict: link calls only to symbols defined in exactly one file, dropping
    /// name-collision false positives. Default false (broader bare-name match).
    #[serde(default)]
    pub strict: bool,
}

#[tool_router(router = router_graph, vis = "pub(crate)")]
impl IndexaMcp {
    /// List a code file's dependencies from the code graph (imports + defined symbols).
    #[tool(
        description = "List a code file's dependencies from the code graph: the modules/paths it imports and the symbols (functions, types, classes) it defines. Requires an absolute path to a file indexed with `indexa deep`."
    )]
    pub(crate) async fn dependencies(
        &self,
        params: Parameters<DependenciesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let edges = store.edges_from(&params.0.path).map_err(mcp_err)?;
        if edges.is_empty() {
            return Ok(ok_text(format!(
                "No code-graph edges for {}. Run `indexa deep` on a code file first.",
                params.0.path
            )));
        }
        let line = |prefix: &str, items: Vec<&str>| {
            items
                .iter()
                .map(|s| format!("  {prefix} {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let imports: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "imports")
            .map(|e| e.to_ref.as_str())
            .collect();
        let defines: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "defines")
            .map(|e| e.to_ref.as_str())
            .collect();
        let calls: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| e.to_ref.as_str())
            .collect();
        let mut out = String::new();
        if !imports.is_empty() {
            out.push_str(&format!(
                "Imports ({}):\n{}\n",
                imports.len(),
                line("→", imports)
            ));
        }
        if !defines.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "Defines ({}):\n{}",
                defines.len(),
                line("•", defines)
            ));
        }
        if !calls.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("\nCalls ({}):\n{}", calls.len(), line("↪", calls)));
        }
        Ok(ok_text(out))
    }

    /// Reverse dependency: which indexed files import a given module/path.
    #[tool(
        description = "Reverse dependency lookup over the code graph: which indexed files import a given module/path (as written in source). Use to find a module's dependents."
    )]
    pub(crate) async fn who_imports(
        &self,
        params: Parameters<WhoImportsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let files = store
            .edges_to("imports", &params.0.module)
            .map_err(mcp_err)?;
        if files.is_empty() {
            return Ok(ok_text(format!(
                "No indexed file imports '{}'.",
                params.0.module
            )));
        }
        // Cap the listing so a ubiquitous module (imported everywhere) can't flood the
        // client's context; report the true total and how many are shown.
        const MAX_SHOWN: usize = 100;
        let total = files.len();
        let body = files
            .iter()
            .take(MAX_SHOWN)
            .map(|p| format!("📄 {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let header = if total > MAX_SHOWN {
            format!(
                "{total} file(s) import '{}' (showing first {MAX_SHOWN}):",
                params.0.module
            )
        } else {
            format!("{total} file(s) import '{}':", params.0.module)
        };
        Ok(ok_text(format!("{header}\n{body}")))
    }

    /// D2 — which files call a given function or method name.
    #[tool(
        description = "D2 code-graph: which indexed files contain a call to the given function or method name (bare, unqualified — e.g. `parse`, `render`, `connect`). Requires `indexa deep` to have been run on source files. Returns up to 100 results."
    )]
    pub(crate) async fn who_calls(
        &self,
        params: Parameters<WhoCallsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let files = store.who_calls(&params.0.symbol, 100).map_err(mcp_err)?;
        if files.is_empty() {
            return Ok(ok_text(format!(
                "No indexed file calls '{}'. Run `indexa deep` on source files first.",
                params.0.symbol
            )));
        }
        let total = files.len();
        let body = files
            .iter()
            .map(|p| format!("📄 {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        // Honest ambiguity note: bare-name matching can't disambiguate a name defined in
        // several files, so the caller list may conflate references to different definitions.
        let defs = store.defines_count(&params.0.symbol).unwrap_or(0);
        let note = if defs > 1 {
            format!(
                "\n\n⚠ '{}' is defined in {defs} files — callers above may target any of them \
                 (bare-name match, no import resolution).",
                params.0.symbol
            )
        } else {
            String::new()
        };
        Ok(ok_text(format!(
            "{total} file(s) call '{}':\n{body}{note}",
            params.0.symbol
        )))
    }

    /// D2 — 1-hop blast radius for a symbol: direct callers and transitive callers.
    #[tool(
        description = "D2 code-graph: compute the blast radius of changing a function or method — returns the direct callers plus files that call any symbol defined in those callers (1-hop transitive). Use to answer 'what breaks if I change X?'. Set `strict: true` to follow only uniquely-defined symbols on the transitive hop (fewer false positives from common names). Returns up to 200 results."
    )]
    pub(crate) async fn blast_radius(
        &self,
        params: Parameters<BlastRadiusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let files = store
            .blast_radius(&params.0.symbol, 200, params.0.strict)
            .map_err(mcp_err)?;
        if files.is_empty() {
            return Ok(ok_text(format!(
                "No blast radius found for '{}'. Run `indexa deep` on source files first.",
                params.0.symbol
            )));
        }
        let total = files.len();
        let body = files
            .iter()
            .map(|p| format!("📄 {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mode = if params.0.strict {
            "strict transitive hop"
        } else {
            "fuzzy — consider strict:true"
        };
        Ok(ok_text(format!(
            "Blast radius of '{}' ({total} file(s)):\n{body}\n\n⚠ Approximate ({mode}): {}.",
            params.0.symbol,
            indexa_core::store::BARE_NAME_CAVEAT
        )))
    }

    /// File-to-file call graph for a scope (the v0.18 signature graph, as text).
    #[tool(
        description = "Build the file-to-file call graph for files under a path scope: an edge 'A → B' means file A calls a function that file B defines. Returns the heaviest edges (most shared symbols) as a 'caller → callee [weight]' list, the most central hub files by weighted PageRank (scored 0–100), plus node/edge counts. Matching is on bare symbol names (case-sensitive); set `strict: true` to keep only uniquely-defined symbols (drops name-collision false positives). Languages: Rust, Python, JS, TS, Go, Java."
    )]
    pub(crate) async fn code_graph(
        &self,
        params: Parameters<CodeGraphParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let CodeGraphParams {
            scope,
            limit,
            strict,
        } = params.0;
        let limit = limit.unwrap_or(200).min(2000);
        let store = self.store()?;
        let graph = store.code_graph(&scope, limit, strict).map_err(mcp_err)?;
        if graph.edges.is_empty() {
            return Ok(ok_text(format!(
                "No call edges under '{scope}'. Run `indexa deep` on source files first."
            )));
        }
        let body = graph
            .edges
            .iter()
            .map(|e| {
                let from = std::path::Path::new(&e.from)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.from.clone());
                let to = std::path::Path::new(&e.to)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.to.clone());
                format!("{from} → {to} [{}]", e.weight)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let trunc = if graph.truncated {
            " (truncated — heaviest shown)"
        } else {
            ""
        };

        // Most-central files by weighted PageRank, scored 0–100 relative to the
        // most central in this scope — surfaces the hub files at a glance.
        let max_pr = graph
            .nodes
            .iter()
            .map(|n| n.pagerank)
            .fold(0.0_f64, f64::max);
        let mut ranked: Vec<_> = graph.nodes.iter().collect();
        ranked.sort_by(|a, b| {
            b.pagerank
                .partial_cmp(&a.pagerank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Full paths here (not basenames): an agent needs to resolve which file
        // to read, and same-named files (e.g. two `ollama.rs`) must not collide.
        let central = ranked
            .iter()
            .take(8)
            .map(|n| {
                let score = if max_pr > 0.0 {
                    (n.pagerank / max_pr * 100.0).round() as i64
                } else {
                    0
                };
                format!("{score:>3}  {}", n.path)
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ok_text(format!(
            "Call graph under '{scope}': {} files, {} edges{trunc}\n\n\
             Most central files (centrality 0–100):\n{central}\n\n\
             Heaviest edges:\n{body}\n\n⚠ Approximate: {}.",
            graph.nodes.len(),
            graph.edges.len(),
            indexa_core::store::BARE_NAME_CAVEAT
        )))
    }

    /// Files related to a file through the call graph.
    #[tool(
        description = "Find files related to a given file through the call graph: files it calls into, or files that call into it, ranked by shared symbol count. Use to discover what to read alongside a file. Bare-name matched (approximate)."
    )]
    pub(crate) async fn related_files(
        &self,
        params: Parameters<RelatedFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = params.0.limit.unwrap_or(15).clamp(1, 100);
        let store = self.store()?;
        let related = store
            .find_related_files(&params.0.path, limit)
            .map_err(mcp_err)?;
        if related.is_empty() {
            return Ok(ok_text(format!(
                "No files related to '{}' (needs a deep-indexed code file with edges).",
                params.0.path
            )));
        }
        let body = related
            .iter()
            .map(|r| format!("{} (shared: {})", r.path, r.shared))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok_text(format!(
            "{} file(s) related to '{}':\n{body}",
            related.len(),
            params.0.path
        )))
    }
}
