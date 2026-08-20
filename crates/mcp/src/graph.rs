//! Code-graph tools: `dependencies`, `who_imports`, `who_calls`,
//! `blast_radius`, `code_graph`, `related_files`, and `changed_impact`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use indexa_core::gitdiff::{self, DiffScope};
use indexa_core::store::{BlastRadius, ResolutionTier, Store};
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use crate::{mcp_err, ok_text, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DependenciesParams {
    /// Absolute path of an indexed code file.
    pub path: String,
    /// Also show `extends`/`implements` heritage edges (2.2) in an "Extends"/"Implements"
    /// section — already stored, just hidden by default so the flat dump stays
    /// byte-identical to before 2.2. Default false.
    #[serde(default)]
    pub include_heritage: bool,
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
    /// Strict: drop the bare-name fallback on the transitive hops — keep only callers
    /// whose call resolves (same-dir/import) to a frontier caller. Default false.
    #[serde(default)]
    pub strict: bool,
    /// How many hops of caller reachability to follow: 1 = direct callers only, 2 = direct +
    /// one transitive hop (default), up to 5 for deeper transitive reach ("what breaks
    /// through chains"). Clamped to 1..=5.
    #[serde(default)]
    pub depth: Option<usize>,
    /// Group results by hop with a risk label (hop 1 "WILL BREAK", hop 2 "LIKELY AFFECTED",
    /// hop 3+ "MAY NEED TESTING") plus a one-line LOW/MEDIUM/HIGH risk summary, instead of a
    /// flat file list. Default false (flat output unchanged).
    #[serde(default)]
    pub grouped: bool,
    /// Also treat a file with an `extends`/`implements` edge to `symbol` as a direct hit
    /// (2.2) — changing a base class/trait breaks its subclasses/implementors just as
    /// directly as changing a called function breaks its callers. Default false.
    #[serde(default)]
    pub include_heritage: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RelatedFilesParams {
    /// Absolute path of the file to find related files for.
    pub path: String,
    /// Max related files to return (default 15).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Also show files that historically changed together with this one in git history
    /// (2.7). Run `indexa graph --compute-co-change` first to populate this — empty until
    /// then. Default false.
    #[serde(default)]
    pub include_co_change: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChangedImpactParams {
    /// Absolute path to the git repository root to diff. Defaults to the first indexed
    /// root if omitted.
    #[serde(default)]
    pub root: Option<String>,
    /// What to diff: `"unstaged"` (default — working tree vs the index), `"staged"`
    /// (index vs HEAD), or a git ref (branch/tag/commit) to diff the working tree against.
    #[serde(default)]
    pub scope: Option<String>,
    /// Strict: drop the bare-name fallback on the transitive hops (same semantics as
    /// `blast_radius`). Default false.
    #[serde(default)]
    pub strict: bool,
    /// How many hops of caller reachability to follow per changed symbol (same semantics
    /// as `blast_radius`). Default 2, clamped to 1..=5.
    #[serde(default)]
    pub depth: Option<usize>,
    /// Also treat a class/trait's subclasses/implementors as direct hits when the diff
    /// touched a heritage-carrying symbol (2.2 semantics). Default false.
    #[serde(default)]
    pub include_heritage: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TracePathParams {
    /// Starting point: an absolute indexed file path, or a bare function/method/symbol
    /// name (resolved to its definer file(s)).
    pub from: String,
    /// Destination: an absolute indexed file path, or a bare symbol name.
    pub to: String,
    /// Max hops to search. Default 10, clamped to 1..=10.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SymbolContextParams {
    /// Bare function, method, class, or type name to build a 360° view of.
    pub symbol: String,
    /// Max callers to list (default 25).
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
    /// Report dependency cycles (Tarjan SCC over the call graph) instead of the edge/hub view —
    /// answers "are there circular dependencies?". Default false.
    #[serde(default)]
    pub cycles: bool,
    /// Report the persisted architecture map (4.6) — named functional-area clusters with a
    /// cohesion score and member files — instead of the edge/hub view. Populated by
    /// `indexa graph --compute-modules`; empty until that's been run at least once. Default
    /// false.
    #[serde(default)]
    pub modules: bool,
}

#[tool_router(router = router_graph, vis = "pub(crate)")]
impl IndexaMcp {
    /// List a code file's dependencies from the code graph (imports + defined symbols).
    #[tool(
        description = "List a code file's dependencies from the code graph: the modules/paths it imports and the symbols (functions, types, classes) it defines. Set `include_heritage: true` to also list `extends`/`implements` edges (2.2) — the base classes/traits this file's types derive from. Requires an absolute path to a file indexed with `indexa deep`."
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
        // Cap each group so a generated/huge file (thousands of imports/symbols/calls) can't
        // flood the client's context; the header always reports the true total and, when
        // truncated, says so — same "showing first N" convention as `who_imports` below.
        const MAX_PER_GROUP: usize = 100;
        let line = |prefix: &str, items: &[&str]| {
            items
                .iter()
                .take(MAX_PER_GROUP)
                .map(|s| format!("  {prefix} {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let trunc = |total: usize| {
            if total > MAX_PER_GROUP {
                format!(", showing first {MAX_PER_GROUP}")
            } else {
                String::new()
            }
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
                "Imports ({}{}):\n{}\n",
                imports.len(),
                trunc(imports.len()),
                line("→", &imports)
            ));
        }
        if !defines.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "Defines ({}{}):\n{}",
                defines.len(),
                trunc(defines.len()),
                line("•", &defines)
            ));
        }
        if !calls.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "\nCalls ({}{}):\n{}",
                calls.len(),
                trunc(calls.len()),
                line("↪", &calls)
            ));
        }
        if params.0.include_heritage {
            let extends: Vec<&str> = edges
                .iter()
                .filter(|e| e.kind == "extends")
                .map(|e| e.to_ref.as_str())
                .collect();
            let implements: Vec<&str> = edges
                .iter()
                .filter(|e| e.kind == "implements")
                .map(|e| e.to_ref.as_str())
                .collect();
            if !extends.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "\nExtends ({}{}):\n{}",
                    extends.len(),
                    trunc(extends.len()),
                    line("▲", &extends)
                ));
            }
            if !implements.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "\nImplements ({}{}):\n{}",
                    implements.len(),
                    trunc(implements.len()),
                    line("◇", &implements)
                ));
            }
        }
        out.push_str(&notes_section(&store, &params.0.path));
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

    /// D2 — blast radius for a symbol: direct callers and transitive callers to `depth` hops,
    /// with each transitive hop resolved (scoped) where possible.
    #[tool(
        description = "D2 code-graph: compute the blast radius of changing a function or method — returns the direct callers plus files whose call to a frontier caller's exported symbol resolves back to that caller (same-dir/import resolution; bare-name matches are kept as a labeled fallback). Use to answer 'what breaks if I change X?'. `depth` controls how many hops of caller reachability to follow: 1 = direct callers only, 2 = direct + one transitive hop (default), up to 5 for transitive reach through chains. Set `strict: true` to drop the bare-name fallback on the transitive hops. Set `grouped: true` to group the file list by hop with a risk label (hop 1 'WILL BREAK', hop 2 'LIKELY AFFECTED', hop 3+ 'MAY NEED TESTING') plus a LOW/MEDIUM/HIGH risk summary, instead of a flat list. Set `include_heritage: true` to also treat a class/trait's subclasses/implementors as direct hits (2.2) — pass a class/trait/interface name as `symbol` to see what breaks if you change it. Returns up to 200 results."
    )]
    pub(crate) async fn blast_radius(
        &self,
        params: Parameters<BlastRadiusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        // Clamp depth to a sane range; default to the legacy 2 (direct + one transitive hop).
        let depth = params.0.depth.unwrap_or(2).clamp(1, 5);
        let radius = store
            .blast_radius_resolved(
                &params.0.symbol,
                200,
                params.0.strict,
                depth,
                params.0.include_heritage,
            )
            .map_err(mcp_err)?;
        if radius.files.is_empty() {
            return Ok(ok_text(format!(
                "No blast radius found for '{}'. Run `indexa deep` on source files first.",
                params.0.symbol
            )));
        }
        let total = radius.files.len();
        let body = if params.0.grouped {
            format_blast_radius_grouped(&radius)
        } else {
            radius
                .files
                .iter()
                .map(|p| format!("📄 {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut out = format!(
            "Blast radius of '{}' (depth {depth}, {total} file(s)):\n{body}\n\n\
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
        out.push_str(&notes_section(&store, &params.0.symbol));
        Ok(ok_text(out))
    }

    /// File-to-file call graph for a scope (the v0.18 signature graph, as text).
    #[tool(
        description = "Build the file-to-file call graph for files under a path scope: an edge 'A → B' means file A calls a function that file B defines. Each call is resolved against the symbol's definition sites (same-file → same-dir → import-matched); unresolvable calls fall back to bare-name matching and are labeled. Returns the heaviest edges (most shared symbols) as a 'caller → callee [weight]' list, the most central hub files by weighted PageRank (scored 0–100), plus node/edge/tier counts. Set `strict: true` to drop the bare-name fallback entirely, `cycles: true` to report dependency cycles (circular call chains), or `modules: true` to report the persisted architecture map (4.6, named functional-area clusters — run `indexa graph --compute-modules` first) instead of the edge/hub view. Languages: Rust, Python, JS, TS, Go, Java, C, C++."
    )]
    pub(crate) async fn code_graph(
        &self,
        params: Parameters<CodeGraphParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let CodeGraphParams {
            scope,
            limit,
            strict,
            cycles,
            modules,
        } = params.0;
        let limit = limit.unwrap_or(200).min(2000);
        let store = self.store()?;

        // Cycle report (Tarjan SCC over the call graph) — a sibling view answering "are there
        // circular dependencies?", matching CLI `indexa graph --cycles`. find_cycles always
        // traverses bare-name edges, so the bare-name caveat always applies here.
        if cycles {
            let found = store.find_cycles(&scope, limit.max(500)).map_err(mcp_err)?;
            if found.is_empty() {
                return Ok(ok_text(format!("No dependency cycles under '{scope}'. ✓")));
            }
            let mut out = format!(
                "{} dependency cycle(s) under '{scope}' (heuristic call resolution — verify):\n",
                found.len()
            );
            for (i, cycle) in found.iter().enumerate() {
                out.push_str(&format!("\nCycle {} ({} files):\n", i + 1, cycle.len()));
                for p in cycle {
                    out.push_str(&format!("  {p}\n"));
                }
            }
            out.push_str(&format!("\n⚠ {}", indexa_core::store::BARE_NAME_CAVEAT));
            return Ok(ok_text(out));
        }

        // Persisted architecture map (4.6) — a sibling view over the `graph_modules` table
        // written by `indexa graph --compute-modules`, matching CLI `indexa graph --modules`.
        if modules {
            let found = store.graph_modules_for_scope(&scope).map_err(mcp_err)?;
            if found.is_empty() {
                return Ok(ok_text(format!(
                    "No architecture-map modules under '{scope}'. Run `indexa graph \
                     --compute-modules` first."
                )));
            }
            let mut out = format!("{} module(s) under '{scope}':\n", found.len());
            for m in &found {
                out.push_str(&format!(
                    "\n📦 {} (cohesion {:.2}, {} file(s)):\n",
                    m.label,
                    m.cohesion,
                    m.members.len()
                ));
                for p in &m.members {
                    out.push_str(&format!("  {p}\n"));
                }
            }
            return Ok(ok_text(out));
        }

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
        description = "Find files related to a given file through the call graph: files it calls into, or files that call into it, ranked by shared symbol count. Each relation is resolved (same-dir/import) where possible; unresolvable links fall back to bare-name matching and are labeled 'bare' (approximate). Set `include_co_change: true` to also list files that historically changed together with this one in git history (2.7) — a behavioral-coupling signal invisible to static analysis; requires `indexa graph --compute-co-change` to have been run first. Use to discover what to read alongside a file."
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
        let co_change = if params.0.include_co_change {
            store
                .co_change_for(&params.0.path, limit)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if related.is_empty() && co_change.is_empty() {
            return Ok(ok_text(format!(
                "No files related to '{}' (needs a deep-indexed code file with edges).",
                params.0.path
            )));
        }
        let mut out = String::new();
        if !related.is_empty() {
            let bare = related
                .iter()
                .filter(|r| r.tier == ResolutionTier::Bare)
                .count();
            let body = related
                .iter()
                .map(|r| format!("{} (shared: {}, {})", r.path, r.shared, r.tier.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&format!(
                "{} file(s) related to '{}':\n{body}",
                related.len(),
                params.0.path
            ));
            // Caveat only when a bare-tier relation is actually present.
            if bare > 0 {
                out.push_str(&format!(
                    "\n\n⚠ {bare} relation(s) are bare-name matched (approximate)."
                ));
            }
        }
        if params.0.include_co_change {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            if co_change.is_empty() {
                out.push_str(
                    "No co-change history (run `indexa graph --compute-co-change` first).",
                );
            } else {
                out.push_str(&format!(
                    "{} file(s) historically co-changed with '{}' (git history):\n{}",
                    co_change.len(),
                    params.0.path,
                    co_change
                        .iter()
                        .map(|c| format!("{} (co-changed {} time(s))", c.path, c.count))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
        Ok(ok_text(out))
    }

    /// 2.5 — 360° view of a symbol: definitions, callers, heritage, and the defining
    /// file's own outgoing references, in one call (replaces who_calls + dependencies +
    /// a manual heritage lookup).
    #[tool(
        description = "360° view of a function/method/class/type name in one call: where it's defined (file, kind, line range when available), who calls it (with resolution tier), who extends/implements it (heritage, 2.2), and what its defining file(s) also import/call (file-granularity approximation of 'outgoing references'). Replaces separately calling who_calls + dependencies + checking heritage by hand. When the name is ambiguous (defined in multiple files) and a `symbol_ambiguity` decision has been answered, the pinned definer is used and noted; otherwise all definers are listed with an ambiguity warning."
    )]
    pub(crate) async fn symbol_context(
        &self,
        params: Parameters<SymbolContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let symbol = &params.0.symbol;
        let limit = params.0.limit.unwrap_or(25).clamp(1, 200);

        let (definers, pinned) = store.definers_with_pin(symbol).map_err(mcp_err)?;
        if definers.is_empty() {
            return Ok(ok_text(format!(
                "No indexed definition found for '{symbol}'. Run `indexa deep` on source \
                 files first."
            )));
        }

        let mut out = format!(
            "Symbol context for '{symbol}':\n\nDefined in ({}):",
            definers.len()
        );
        for d in &definers {
            let syms = store.symbols_in_file(d).unwrap_or_default();
            match syms.into_iter().find(|s| &s.name == symbol) {
                Some(s) => out.push_str(&format!(
                    "\n📄 {d}:{}-{} ({})",
                    s.start_line, s.end_line, s.kind
                )),
                None => out.push_str(&format!("\n📄 {d}")),
            }
        }
        if pinned {
            out.push_str("\n(disambiguated via a decided symbol_ambiguity pin)");
        } else if definers.len() > 1 {
            out.push_str(&format!(
                "\n⚠ '{symbol}' has {} definitions — ambiguous; answer the symbol_ambiguity \
                 decision (list_open_decisions) to pin one.",
                definers.len()
            ));
        }

        let callers = store.who_calls_resolved(symbol, limit).map_err(mcp_err)?;
        if callers.is_empty() {
            out.push_str("\n\nCalled by: none found.");
        } else {
            // `who_calls_resolved` is SQL-LIMIT-capped at `limit`, not a real total — when the
            // result hits the cap exactly, say so instead of implying `callers.len()` is the
            // whole count (there may be more callers beyond `limit`).
            let trunc = if callers.len() == limit {
                ", truncated — raise `limit` for more"
            } else {
                ""
            };
            out.push_str(&format!("\n\nCalled by ({}{trunc}):", callers.len()));
            for c in &callers {
                out.push_str(&format!("\n  ↩ {} [{}]", c.path, c.tier.label()));
            }
        }

        let extenders = store.edges_to("extends", symbol).map_err(mcp_err)?;
        let implementers = store.edges_to("implements", symbol).map_err(mcp_err)?;
        if !extenders.is_empty() {
            out.push_str(&format!("\n\nExtended by ({}):", extenders.len()));
            for e in &extenders {
                out.push_str(&format!("\n  ▲ {e}"));
            }
        }
        if !implementers.is_empty() {
            out.push_str(&format!("\n\nImplemented by ({}):", implementers.len()));
            for i in &implementers {
                out.push_str(&format!("\n  ◇ {i}"));
            }
        }

        let mut outgoing_imports: BTreeSet<String> = BTreeSet::new();
        let mut outgoing_calls: BTreeSet<String> = BTreeSet::new();
        for d in &definers {
            for e in store.edges_from(d).unwrap_or_default() {
                match e.kind.as_str() {
                    "imports" => {
                        outgoing_imports.insert(e.to_ref);
                    }
                    "calls" => {
                        outgoing_calls.insert(e.to_ref);
                    }
                    _ => {}
                }
            }
        }
        if !outgoing_imports.is_empty() {
            out.push_str(&format!(
                "\n\nDefining file also imports ({}):",
                outgoing_imports.len()
            ));
            for i in outgoing_imports.iter().take(20) {
                out.push_str(&format!("\n  → {i}"));
            }
        }
        if !outgoing_calls.is_empty() {
            out.push_str(&format!(
                "\n\nDefining file also calls ({}):",
                outgoing_calls.len()
            ));
            for c in outgoing_calls.iter().take(20) {
                out.push_str(&format!("\n  ↪ {c}"));
            }
        }
        out.push_str(&notes_section(&store, symbol));

        Ok(ok_text(out))
    }

    /// 2.4 — BFS shortest callee-direction path between two symbols/files.
    #[tool(
        description = "Find the shortest call-graph path from one symbol/file to another — \"how does A reach B?\". `from`/`to` each accept an absolute indexed file path or a bare symbol name (resolved to its definer file(s)). Only structurally-confirmed edges (same-file/same-dir/import resolution) are traversed — bare-name matches are too unreliable to report as a concrete path, so no result found means 'no confirmed path within max_depth', not 'definitely unreachable'. Returns the ordered chain of files from `from` to `to`, each hop labeled with how it resolved. `max_depth` bounds the search (default 10, max 10)."
    )]
    pub(crate) async fn trace_path(
        &self,
        params: Parameters<TracePathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let max_depth = params.0.max_depth.unwrap_or(10).clamp(1, 10);
        let path = store
            .trace_path(&params.0.from, &params.0.to, max_depth)
            .map_err(mcp_err)?;
        let Some(hops) = path else {
            return Ok(ok_text(format!(
                "No confirmed path found from '{}' to '{}' within {max_depth} hop(s). \
                 Either it doesn't exist, exceeds the depth limit, or only resolves via \
                 bare-name matching (not traversed as a concrete path).",
                params.0.from, params.0.to
            )));
        };
        let body = hops
            .iter()
            .enumerate()
            .map(|(i, h)| {
                if i == 0 {
                    format!("📄 {}", h.path)
                } else {
                    format!("  ↪ [{}] {}", h.tier.label(), h.path)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok_text(format!(
            "Path from '{}' to '{}' ({} hop(s)):\n{body}",
            params.0.from,
            params.0.to,
            hops.len().saturating_sub(1)
        )))
    }

    /// 2.3 — diff → changed spans → symbols → blast radius, in one call.
    #[tool(
        description = "\"What did I just touch and what does it break?\" — diffs a git repo, maps the changed line ranges to symbols (via the symbols table, populated by `indexa deep`), and runs `blast_radius` on each changed symbol, merging the results (grouped by hop, same risk labeling as `blast_radius`'s `grouped: true`). Set `scope` to `\"unstaged\"` (default — working tree vs the index), `\"staged\"` (index vs HEAD), or a git ref (branch/tag/commit) to diff the working tree against that ref. `root` defaults to the first indexed root; pass it explicitly in a multi-root index. Changed files with no symbol-level data (non-code files, or not yet `indexa deep`-indexed) are reported separately via file-level related-files lookup. `strict`, `depth`, and `include_heritage` have the same meaning as on `blast_radius`."
    )]
    pub(crate) async fn changed_impact(
        &self,
        params: Parameters<ChangedImpactParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let store = self.store()?;
        let root =
            match params.0.root {
                Some(r) => r,
                None => match store.root_paths().map_err(mcp_err)?.into_iter().next() {
                    Some(r) => r,
                    None => return Ok(ok_text(
                        "No indexed roots — run `indexa scan` first, or pass `root` explicitly.",
                    )),
                },
            };
        let scope_str = params.0.scope.clone().unwrap_or_default();
        let scope = DiffScope::parse(&scope_str);
        let hunks = gitdiff::changed_hunks(Path::new(&root), &scope).map_err(mcp_err)?;
        if hunks.is_empty() {
            return Ok(ok_text(format!(
                "No changes found under '{root}' ({}).",
                scope_label(&scope)
            )));
        }
        let depth = params.0.depth.unwrap_or(2).clamp(1, 5);
        let (changed_symbols, file_level) =
            map_hunks_to_symbols(&store, &hunks).map_err(mcp_err)?;
        let files_touched: BTreeSet<&str> = hunks.iter().map(|h| h.path.as_str()).collect();

        let mut out = format!(
            "Changed under '{root}' ({}): {} file(s), {} hunk(s)",
            scope_label(&scope),
            files_touched.len(),
            hunks.len(),
        );

        if !changed_symbols.is_empty() {
            let names: Vec<String> = changed_symbols.iter().map(|(_, n)| n.clone()).collect();
            let radius = store
                .changed_impact(
                    &names,
                    200,
                    params.0.strict,
                    depth,
                    params.0.include_heritage,
                )
                .map_err(mcp_err)?;
            out.push_str(&format!(
                "\n\nChanged symbols ({}): {}",
                changed_symbols.len(),
                changed_symbols
                    .iter()
                    .map(|(f, n)| format!("{n} ({f})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if radius.files.is_empty() {
                out.push_str(
                    "\n\nNo callers found for the changed symbols — nothing else appears \
                     to depend on them.",
                );
            } else {
                out.push_str(&format!("\n\n{}", format_blast_radius_grouped(&radius)));
                if radius.bare_transitive > 0 {
                    out.push_str(&format!(
                        "\n⚠ {} transitive file(s) are approximate: {}.",
                        radius.bare_transitive,
                        indexa_core::store::BARE_NAME_CAVEAT
                    ));
                }
            }
        }

        if !file_level.is_empty() {
            out.push_str(&format!(
                "\n\nChanged file(s) with no symbol-level detail ({}) — file-level related \
                 files (approximate; run `indexa deep` for symbol-level precision):",
                file_level.len()
            ));
            for f in &file_level {
                let related = store.find_related_files_resolved(f, 10).unwrap_or_default();
                if related.is_empty() {
                    out.push_str(&format!("\n📄 {f} — no related files found"));
                } else {
                    out.push_str(&format!("\n📄 {f} →"));
                    for r in &related {
                        out.push_str(&format!("\n    {} ({})", r.path, r.tier.as_str()));
                    }
                }
            }
        }

        if changed_symbols.is_empty() && file_level.is_empty() {
            out.push_str(
                "\n\nNo indexed changes to report — the changed files may not be code, or \
                 `indexa deep` hasn't run since the edit.",
            );
        }

        Ok(ok_text(out))
    }
}

/// Hop → risk label (GitNexus's `impact` contract): hop 1 = direct callers (will break
/// immediately), hop 2 = one transitive step (likely affected), hop 3+ = further steps.
fn hop_risk_label(hop: usize) -> &'static str {
    match hop {
        1 => "WILL BREAK",
        2 => "LIKELY AFFECTED",
        _ => "MAY NEED TESTING",
    }
}

/// Render a `blast_radius` result grouped by hop with a risk label per group, plus an
/// overall LOW/MEDIUM/HIGH risk summary line — the `grouped: true` body.
fn format_blast_radius_grouped(radius: &BlastRadius) -> String {
    let mut out = format!(
        "risk: {} ({} direct caller(s))",
        radius.risk().as_str(),
        radius.direct
    );
    for (hop, files) in radius.grouped_by_hop() {
        out.push_str(&format!(
            "\n\nhop {hop} — {} ({} file(s)):",
            hop_risk_label(hop),
            files.len()
        ));
        for f in &files {
            out.push_str(&format!("\n📄 {f}"));
        }
    }
    out
}

/// Render a "Notes" section for anchored notes (2.6) on `anchor` — a code path or bare
/// symbol name — or an empty string when none exist. Best-effort: a lookup error is
/// swallowed (fails open) rather than failing the whole tool call over an optional extra.
fn notes_section(store: &Store, anchor: &str) -> String {
    let notes = store.note_anchors_for(anchor).unwrap_or_default();
    if notes.is_empty() {
        return String::new();
    }
    let mut out = format!("\n\nNotes ({}):", notes.len());
    for n in &notes {
        out.push_str(&format!(
            "\n  📝 {} ({}) — {}",
            n.title, n.pack, n.note_path
        ));
    }
    out
}

/// Human label for a [`DiffScope`], for `changed_impact` output.
fn scope_label(scope: &DiffScope) -> String {
    match scope {
        DiffScope::Unstaged => "unstaged".to_owned(),
        DiffScope::Staged => "staged".to_owned(),
        DiffScope::Ref(r) => format!("vs {r}"),
    }
}

/// `(file, symbol name)` pairs found by mapping changed hunks to symbols.
type ChangedSymbols = Vec<(String, String)>;

/// Group `hunks` by file and map each file's changed line ranges to symbol names via
/// [`Store::symbols_overlapping`]. Returns `(changed_symbols, file_level)`: symbols found
/// (as `(file, name)` pairs, deduped per file) plus the files whose hunks matched no
/// symbol at all (no symbol-level data — the caller falls back to file-level relations).
fn map_hunks_to_symbols(
    store: &Store,
    hunks: &[gitdiff::ChangedHunk],
) -> anyhow::Result<(ChangedSymbols, Vec<String>)> {
    let mut by_file: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for h in hunks {
        by_file
            .entry(h.path.as_str())
            .or_default()
            .push((h.start_line, h.end_line));
    }
    let mut changed_symbols = Vec::new();
    let mut file_level = Vec::new();
    for (file, ranges) in by_file {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for (start, end) in ranges {
            for sym in store.symbols_overlapping(file, start, end)? {
                names.insert(sym.name);
            }
        }
        if names.is_empty() {
            file_level.push(file.to_owned());
        } else {
            changed_symbols.extend(names.into_iter().map(|n| (file.to_owned(), n)));
        }
    }
    Ok((changed_symbols, file_level))
}
