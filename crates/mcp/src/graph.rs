//! Code-graph tools: `dependencies`, `who_imports`, `who_calls`,
//! `blast_radius`, `code_graph`, and `related_files`.

use indexa_core::store::ResolutionTier;
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
    /// Strict: drop the bare-name fallback on the transitive hop — keep only callers
    /// whose call resolves (same-dir/import) to a direct caller. Default false.
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
    /// Strict: drop the bare-name fallback tier — keep only edges whose call resolved
    /// via same-dir or import matching. Default false (bare edges kept, labeled).
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

    /// D2 — which files call a given function or method name, grouped by how each
    /// call resolved (same-file / same-dir / import / bare).
    #[tool(
        description = "D2 code-graph: which indexed files contain a call to the given function or method name (bare, unqualified — e.g. `parse`, `render`, `connect`). Each caller is resolved against the name's definition sites (same-file → same-dir → import-matched → bare-name fallback) and the output is grouped by that tier; only the bare group is approximate. Requires `indexa deep` to have been run on source files. Returns up to 100 results."
    )]
    pub(crate) async fn who_calls(
        &self,
        params: Parameters<WhoCallsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let resolved = store
            .who_calls_resolved(&params.0.symbol, 100)
            .map_err(mcp_err)?;
        if resolved.is_empty() {
            return Ok(ok_text(format!(
                "No indexed file calls '{}'. Run `indexa deep` on source files first.",
                params.0.symbol
            )));
        }
        let total = resolved.len();
        // Group by tier, best first. Resolved tiers show their target definition file(s);
        // an arrow-less entry means the caller IS the definer (same-file).
        let mut out = format!("{total} file(s) call '{}':", params.0.symbol);
        let groups: &[(ResolutionTier, &str)] = &[
            (
                ResolutionTier::SameFile,
                "same-file — call their own definition",
            ),
            (
                ResolutionTier::SameDir,
                "same-dir — resolved to a definition beside the caller",
            ),
            (
                ResolutionTier::Import,
                "import-resolved — definition matched an import",
            ),
            (
                ResolutionTier::Bare,
                "bare-name — unresolved, may target any definer",
            ),
        ];
        let mut bare_count = 0usize;
        for (tier, label) in groups {
            let members: Vec<_> = resolved.iter().filter(|r| r.tier == *tier).collect();
            if members.is_empty() {
                continue;
            }
            if *tier == ResolutionTier::Bare {
                bare_count = members.len();
            }
            out.push_str(&format!("\n\n{label} ({}):", members.len()));
            for m in members {
                if m.targets.is_empty() || m.targets == [m.path.clone()] {
                    out.push_str(&format!("\n📄 {}", m.path));
                } else {
                    out.push_str(&format!("\n📄 {} → {}", m.path, m.targets.join(", ")));
                }
            }
        }
        // The ambiguity caveat applies ONLY to the bare remainder.
        let defs = store.defines_count(&params.0.symbol).unwrap_or(0);
        if bare_count > 0 && defs > 1 {
            out.push_str(&format!(
                "\n\n⚠ '{}' is defined in {defs} files — the bare-name callers above may \
                 target any of them.",
                params.0.symbol
            ));
        }
        Ok(ok_text(out))
    }

    /// D2 — 1-hop blast radius for a symbol: direct callers and transitive callers,
    /// with the transitive hop resolved (scoped) where possible.
    #[tool(
        description = "D2 code-graph: compute the blast radius of changing a function or method — returns the direct callers plus files whose call to one of those callers' exported symbols resolves back to that caller (same-dir/import resolution; bare-name matches are kept as a labeled fallback). Use to answer 'what breaks if I change X?'. Set `strict: true` to drop the bare-name fallback on the transitive hop. Returns up to 200 results."
    )]
    pub(crate) async fn blast_radius(
        &self,
        params: Parameters<BlastRadiusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let radius = store
            .blast_radius_resolved(&params.0.symbol, 200, params.0.strict)
            .map_err(mcp_err)?;
        if radius.files.is_empty() {
            return Ok(ok_text(format!(
                "No blast radius found for '{}'. Run `indexa deep` on source files first.",
                params.0.symbol
            )));
        }
        let total = radius.files.len();
        let body = radius
            .files
            .iter()
            .map(|p| format!("📄 {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut out = format!(
            "Blast radius of '{}' ({total} file(s)):\n{body}\n\n\
             direct callers: {} · transitive: {} resolution-confirmed + {} bare-name{}",
            params.0.symbol,
            radius.direct,
            radius.scoped_transitive,
            radius.bare_transitive,
            if params.0.strict {
                " (strict: bare fallback disabled)"
            } else {
                ""
            }
        );
        // Caveats apply only to the name-matched parts: the bare transitive remainder,
        // and the direct set when the input name has several definitions.
        if radius.bare_transitive > 0 {
            out.push_str(&format!(
                "\n⚠ {} transitive file(s) are approximate: {}.",
                radius.bare_transitive,
                indexa_core::store::BARE_NAME_CAVEAT
            ));
        }
        let defs = store.defines_count(&params.0.symbol).unwrap_or(0);
        if defs > 1 {
            out.push_str(&format!(
                "\n⚠ '{}' is defined in {defs} files — direct callers are name-matched and \
                 may target any of them (see who_calls for per-caller resolution).",
                params.0.symbol
            ));
        }
        Ok(ok_text(out))
    }

    /// File-to-file call graph for a scope (the v0.18 signature graph, as text).
    #[tool(
        description = "Build the file-to-file call graph for files under a path scope: an edge 'A → B' means file A calls a function that file B defines. Each call is resolved against the symbol's definition sites (same-file → same-dir → import-matched); unresolvable calls fall back to bare-name matching and are labeled. Returns the heaviest edges (most shared symbols) as a 'caller → callee [weight]' list, the most central hub files by weighted PageRank (scored 0–100), plus node/edge/tier counts. Set `strict: true` to drop the bare-name fallback entirely. Languages: Rust, Python, JS, TS, Go, Java."
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
        let scoped = store
            .code_graph_scoped(&scope, limit, strict)
            .map_err(mcp_err)?;
        let graph = &scoped.graph;
        if graph.edges.is_empty() {
            return Ok(ok_text(format!(
                "No call edges under '{scope}'. Run `indexa deep` on source files first."
            )));
        }
        let body = graph
            .edges
            .iter()
            .zip(&scoped.edge_tiers)
            .map(|(e, tier)| {
                let from = std::path::Path::new(&e.from)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.from.clone());
                let to = std::path::Path::new(&e.to)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.to.clone());
                // Only the bare remainder is approximate — flag it inline.
                let mark = if *tier == ResolutionTier::Bare {
                    " (bare)"
                } else {
                    ""
                };
                format!("{from} → {to} [{}]{mark}", e.weight)
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

        // Tier summary; the bare-name caveat applies ONLY to the bare remainder.
        let count = |t: ResolutionTier| scoped.edge_tiers.iter().filter(|x| **x == t).count();
        let (same_dir, import, bare) = (
            count(ResolutionTier::SameDir),
            count(ResolutionTier::Import),
            count(ResolutionTier::Bare),
        );
        let caveat = if bare > 0 {
            format!(
                "\n\n⚠ {bare} bare-name edge(s) are approximate: {}.",
                indexa_core::store::BARE_NAME_CAVEAT
            )
        } else {
            "\n\nNo bare-name matches in this view (same-dir edges are proximity-matched; \
             same-file/import are structural)."
                .to_owned()
        };
        Ok(ok_text(format!(
            "Call graph under '{scope}': {} files, {} edges{trunc}\n\
             edges: {} scoped ({same_dir} same-dir, {import} import-resolved) + {bare} bare-name\n\n\
             Most central files (centrality 0–100):\n{central}\n\n\
             Heaviest edges:\n{body}{caveat}",
            graph.nodes.len(),
            graph.edges.len(),
            same_dir + import,
        )))
    }

    /// Files related to a file through the call graph, with resolution tiers.
    #[tool(
        description = "Find files related to a given file through the call graph: files it calls into, or files that call into it, ranked by shared symbol count. Each relation is resolved (same-dir/import) where possible; unresolvable links fall back to bare-name matching and are labeled 'bare' (approximate). Use to discover what to read alongside a file."
    )]
    pub(crate) async fn related_files(
        &self,
        params: Parameters<RelatedFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = params.0.limit.unwrap_or(15).clamp(1, 100);
        let store = self.store()?;
        let related = store
            .find_related_files_resolved(&params.0.path, limit)
            .map_err(mcp_err)?;
        if related.is_empty() {
            return Ok(ok_text(format!(
                "No files related to '{}' (needs a deep-indexed code file with edges).",
                params.0.path
            )));
        }
        let bare = related
            .iter()
            .filter(|r| r.tier == ResolutionTier::Bare)
            .count();
        let body = related
            .iter()
            .map(|r| format!("{} (shared: {}, {})", r.path, r.shared, r.tier.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        // Caveat only when a bare-tier relation is actually present.
        let note = if bare > 0 {
            format!("\n\n⚠ {bare} relation(s) are bare-name matched (approximate).")
        } else {
            String::new()
        };
        Ok(ok_text(format!(
            "{} file(s) related to '{}':\n{body}{note}",
            related.len(),
            params.0.path
        )))
    }
}
