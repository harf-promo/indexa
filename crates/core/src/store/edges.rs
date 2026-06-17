//! Code-relationship-graph edge writes and queries (the `edges` table).
//!
//! D1: per-file `imports` and `defines` edges.
//! D2: per-file `calls` edges — function/method names called by a file.
//!
//! v0.25 — **scoped call resolution** at query time: a `calls X` edge from file A is
//! resolved against X's definition sites in tier order (same-file → same-dir →
//! import-linked → bare-name fallback) before any surface renders it. Edge *recording*
//! is unchanged, so existing indexes get the precision win without a re-deep.

use super::search::like_prefix;
use super::{CodeGraph, CodeGraphEdge, CodeGraphNode, EdgeRecord, RelatedFile, Store};
use anyhow::Result;
use rusqlite::params;
use std::collections::{BTreeSet, HashMap, HashSet};

/// The honesty caveat for the **bare remainder** of D2 call-graph results (CLI, MCP,
/// web). Since v0.25 most edges resolve via scoped tiers (same-file/same-dir/import);
/// surfaces must show this line only when bare-name edges are actually present.
/// Full discussion in docs/methodology.md.
pub const BARE_NAME_CAVEAT: &str = "bare-name edges match call→define by symbol name \
only (case-sensitive, no import resolution) — a name defined in multiple files \
conflates their callers; strict mode drops bare-name edges entirely";

/// How a `calls` edge was resolved to its definition site(s). Variants are ordered
/// best→worst, so `Ord` picks the most trustworthy tier (`min`) when aggregating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResolutionTier {
    /// The caller itself defines the symbol — the call binds to the local definition
    /// and must NOT fan out to same-named definitions elsewhere.
    SameFile,
    /// Definition(s) found in the caller's own directory.
    SameDir,
    /// The definer's path matched one of the caller's `imports` strings.
    Import,
    /// Name-only match (the historical behavior) — approximate; carries
    /// [`BARE_NAME_CAVEAT`].
    Bare,
}

impl ResolutionTier {
    /// Stable wire/JSON form (snake_case).
    pub fn as_str(self) -> &'static str {
        match self {
            ResolutionTier::SameFile => "same_file",
            ResolutionTier::SameDir => "same_dir",
            ResolutionTier::Import => "import",
            ResolutionTier::Bare => "bare",
        }
    }

    /// Human display label (hyphenated) for CLI tables and UI badges.
    pub fn label(self) -> &'static str {
        match self {
            ResolutionTier::SameFile => "same-file",
            ResolutionTier::SameDir => "same-dir",
            ResolutionTier::Import => "import",
            ResolutionTier::Bare => "bare",
        }
    }

    /// Whether this tier is the approximate, name-only fallback (carries
    /// [`BARE_NAME_CAVEAT`]). The scoped tiers (same-file/import/same-dir) are
    /// structural or proximity-backed.
    pub fn is_bare(self) -> bool {
        matches!(self, ResolutionTier::Bare)
    }
}

/// One caller of a symbol, with how its call resolved. For [`ResolutionTier::Bare`]
/// `targets` is empty (every bare caller would carry the identical full definer list —
/// use [`Store::defines_count`] for the count instead).
#[derive(Debug, Clone)]
pub struct ResolvedCaller {
    pub path: String,
    pub tier: ResolutionTier,
    /// The definition file(s) this caller's call resolved to (empty for `Bare`).
    pub targets: Vec<String>,
}

/// [`CodeGraph`] plus the resolution tier of each edge. `edge_tiers[i]` describes
/// `graph.edges[i]` (parallel array — `CodeGraphEdge` itself is shared with surfaces
/// that don't know about tiers). Cross-file edges never carry `SameFile` (a same-file
/// resolution produces no cross-file edge — that's the false-positive killer).
#[derive(Debug, Clone)]
pub struct ScopedCodeGraph {
    pub graph: CodeGraph,
    pub edge_tiers: Vec<ResolutionTier>,
}

/// Result of [`Store::blast_radius_resolved`]: the affected files plus how the
/// transitive hop resolved. Counts describe the pre-truncation set.
#[derive(Debug, Clone)]
pub struct BlastRadius {
    /// Direct callers ∪ included transitive callers, sorted, capped at `limit`.
    pub files: Vec<String>,
    /// Direct callers of the input symbol (name-matched — a bare input name has no
    /// definer to disambiguate against; see `who_calls_resolved` for per-caller tiers).
    pub direct: usize,
    /// Transitive callers whose call **resolved** (same-dir/import) to a direct caller.
    pub scoped_transitive: usize,
    /// Transitive callers kept on the bare-name fallback only (dropped when `strict`).
    pub bare_transitive: usize,
}

/// A related file with the best resolution tier that linked it.
#[derive(Debug, Clone)]
pub struct ResolvedRelatedFile {
    pub path: String,
    pub shared: usize,
    pub tier: ResolutionTier,
}

// ── path helpers (lexical only — index paths may not exist on this machine) ──────

fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Lexically resolve `.` / `..` segments ("/a/b/../c" → "/a/c"). No fs access:
/// the indexed tree may live on another machine or have been deleted since.
fn lexical_normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

// ── import → file matching (tier 3) ──────────────────────────────────────────────
//
// A small, honest per-language matcher over the import strings the parser already
// records. Exactly these forms resolve (anything else contributes nothing and the
// call falls through to the bare tier):
//
//   JS/TS    relative specifiers `./x`, `../y/z` — joined to the caller's dir, tried
//            with the usual extensions and `/index.*`. Bare/package specifiers
//            (`react`, `@scope/pkg`, `lodash/merge`) do not resolve.
//   Rust     `crate::a::b` — matched as `<crate-src>/a/b.rs` or `.../a/b/mod.rs`,
//            where `<crate-src>` is the caller's nearest `/src/` ancestor; and
//            `super::a` (one dir up per extra `super`). The path minus its final
//            segment is also tried (`use crate::a::b::item`). External-crate paths
//            (`std::fs`, `other_crate::x`), `self::`, and use-renames' *aliases* do
//            not resolve.
//   Python   dotted absolute modules `a.b` — suffix-matched as `a/b.py` or
//            `a/b/__init__.py`. Relative imports are recorded by the parser without
//            their leading dots, so they degrade to a (broader) suffix match.
//   Go/Java  not resolved (package paths / FQCNs don't map to files lexically) —
//            their calls still get same-file / same-dir / bare tiers.

const JS_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];

/// A caller's import strings pre-resolved into absolute candidate paths (JS relative,
/// Rust `crate::`/`super::`) and path suffixes (Python dotted — no anchor to resolve
/// against). A definer matches when it equals a candidate or ends with a suffix.
struct ImportTargets {
    absolute: HashSet<String>,
    suffixes: Vec<String>,
}

impl ImportTargets {
    fn matches(&self, path: &str) -> bool {
        self.absolute.contains(path) || self.suffixes.iter().any(|s| path.ends_with(s.as_str()))
    }
}

fn add_js_relative(caller_dir: &str, spec: &str, abs: &mut HashSet<String>) {
    let joined = lexical_normalize(&format!("{caller_dir}/{spec}"));
    // Explicit extension → take as-is; otherwise try the usual resolution
    // candidates: appended extensions, then a directory index file.
    if JS_EXTS.iter().any(|e| joined.ends_with(&format!(".{e}"))) {
        abs.insert(joined);
        return;
    }
    for e in JS_EXTS {
        abs.insert(format!("{joined}.{e}"));
        abs.insert(format!("{joined}/index.{e}"));
    }
}

fn add_rust_use(caller: &str, spec: &str, abs: &mut HashSet<String>) {
    // `use crate::a::{B, c}` / `use crate::a::*` → module path `crate::a`.
    let path_part = spec
        .split("::{")
        .next()
        .unwrap_or(spec)
        .trim_end_matches("::*");
    let segs: Vec<&str> = path_part
        .split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let (root_dir, rest): (String, &[&str]) = match segs.first() {
        Some(&"crate") => {
            // Crate root = the caller's nearest `src/` ancestor (standard cargo
            // layout). Callers outside any `src/` don't resolve `crate::` paths.
            let Some(pos) = caller.rfind("/src/") else {
                return;
            };
            (caller[..pos + "/src/".len()].to_owned(), &segs[1..])
        }
        Some(&"super") => {
            // The module parent of `m/file.rs` is the `m/` dir itself; of `m/mod.rs`
            // it's the dir above. Each extra `super` pops one more directory.
            let mut dir = parent_dir(caller).to_owned();
            if file_name(caller) == "mod.rs" {
                dir = parent_dir(&dir).to_owned();
            }
            let mut i = 1;
            while segs.get(i) == Some(&"super") {
                dir = parent_dir(&dir).to_owned();
                i += 1;
            }
            (format!("{dir}/"), &segs[i..])
        }
        // External crates, `std::`, `self::` — not resolved (documented above).
        _ => return,
    };
    if rest.is_empty() {
        return;
    }
    // Try the full path as a module, and the path minus the trailing segment
    // (`use crate::a::b::item` — `item` lives inside module `a/b`).
    for end in [rest.len(), rest.len() - 1] {
        if end == 0 {
            continue;
        }
        let joined = rest[..end].join("/");
        abs.insert(format!("{root_dir}{joined}.rs"));
        abs.insert(format!("{root_dir}{joined}/mod.rs"));
    }
}

fn add_py_dotted(spec: &str, suffixes: &mut Vec<String>) {
    // `import a.b` / `from a.b import c` both record module `a.b`. Only plain
    // identifier segments qualify — anything else is not a Python module path.
    let segs: Vec<&str> = spec.split('.').collect();
    if segs
        .iter()
        .any(|s| s.is_empty() || !s.chars().all(|c| c.is_alphanumeric() || c == '_'))
    {
        return;
    }
    let joined = segs.join("/");
    // Single-segment specs resolve ONLY to the directory-package form:
    // `import utils` suffix-matching any utils.py across a whole-disk index is
    // a false Import-tier promotion (and slash-free single segments are also
    // how Go packages, bare JS specifiers, and Java FQCN fragments arrive
    // here). A `<name>/__init__.py` package is structurally specific enough to
    // keep; same-dir handles bare local sibling modules.
    if segs.len() >= 2 {
        suffixes.push(format!("/{joined}.py"));
    }
    suffixes.push(format!("/{joined}/__init__.py"));
}

/// Build the matchable target set for one caller from its recorded import strings.
/// Dispatch is by the *shape* of the string (the edges table has no language column):
/// `./`/`../` → JS relative; `::` → Rust path; bare dotted name → Python module.
fn import_targets(caller: &str, imports: &[String]) -> ImportTargets {
    let dir = parent_dir(caller);
    let mut absolute = HashSet::new();
    let mut suffixes = Vec::new();
    for spec in imports {
        if spec.starts_with("./") || spec.starts_with("../") {
            add_js_relative(dir, spec, &mut absolute);
        } else if spec.contains("::") {
            add_rust_use(caller, spec, &mut absolute);
        } else if !spec.is_empty() && !spec.contains('/') {
            add_py_dotted(spec, &mut suffixes);
        }
        // Everything else (bare JS specifiers, Go package paths, …) → no resolution.
    }
    ImportTargets { absolute, suffixes }
}

// ── definition-site index + the tier resolver ─────────────────────────────────────

/// Definers of one symbol, indexed for O(1) same-file / same-dir checks. `all` is
/// sorted+deduped so bare-tier output is deterministic.
struct DefinerIndex {
    all: Vec<String>,
    set: HashSet<String>,
    by_dir: HashMap<String, Vec<String>>,
}

impl DefinerIndex {
    fn new(mut all: Vec<String>) -> Self {
        all.sort();
        all.dedup();
        let set: HashSet<String> = all.iter().cloned().collect();
        let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
        for p in &all {
            by_dir
                .entry(parent_dir(p).to_owned())
                .or_default()
                .push(p.clone());
        }
        Self { all, set, by_dir }
    }
}

/// Rank a caller's `calls` edge against the symbol's definition sites. The same-file
/// tier is the **caller's responsibility** to check first (contexts differ: code_graph
/// drops the edge as a self-edge, who_calls reports it) — this resolves tiers 2–4.
fn resolve_call(
    caller: &str,
    imports: &ImportTargets,
    defs: &DefinerIndex,
) -> (ResolutionTier, Vec<String>) {
    // Import BEFORE same-dir: an explicit import is stronger evidence than
    // directory proximity. The reverse order silently dropped import-confirmed
    // true edges whenever a same-named sibling existed — and promoted the
    // (possibly wrong) sibling to a trusted tier.
    let v: Vec<String> = defs
        .all
        .iter()
        .filter(|p| p.as_str() != caller && imports.matches(p))
        .cloned()
        .collect();
    if !v.is_empty() {
        return (ResolutionTier::Import, v);
    }
    if let Some(neighbors) = defs.by_dir.get(parent_dir(caller)) {
        let v: Vec<String> = neighbors
            .iter()
            .filter(|p| p.as_str() != caller)
            .cloned()
            .collect();
        if !v.is_empty() {
            return (ResolutionTier::SameDir, v);
        }
    }
    (
        ResolutionTier::Bare,
        defs.all
            .iter()
            .filter(|p| p.as_str() != caller)
            .cloned()
            .collect(),
    )
}

/// Tarjan's strongly-connected-components, iterative (no recursion → no stack-overflow risk
/// on deep graphs). `adj[v]` lists v's out-neighbors. Returns every SCC (including singletons);
/// the caller keeps those with len > 1 as cycles.
fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut idx = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    let mut next_index = 0usize;

    for start in 0..n {
        if idx[start] != usize::MAX {
            continue;
        }
        // DFS frames: (node, next-child-pointer).
        let mut call_stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, ci)) = call_stack.last() {
            if ci == 0 {
                idx[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if ci < adj[v].len() {
                let w = adj[v][ci];
                call_stack.last_mut().unwrap().1 += 1;
                if idx[w] == usize::MAX {
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(idx[w]);
                }
            } else {
                // Finished v: if it's an SCC root, pop the component.
                if low[v] == idx[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    sccs
}

impl Store {
    /// A `defines` symbol present in more than this many files is treated as a generic
    /// name (`new`/`from`/`default`) and excluded from [`Self::code_graph`] — it bounds the
    /// resolution work's worst case and removes low-signal noise.
    const CODE_GRAPH_COMMON_SYMBOL_CAP: i64 = 25;

    /// Replace every edge originating at each file in the batch (delete-by-`from_path`
    /// then insert), mirroring [`upsert_chunks`](Self::upsert_chunks) so a re-`deep` of a
    /// file refreshes its graph rather than accumulating stale edges. `INSERT OR IGNORE`
    /// collapses duplicates against the composite primary key.
    /// Every edge in the graph (for snapshot export). Ordered for stable output.
    pub fn all_edges(&self) -> Result<Vec<EdgeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_path, kind, to_ref FROM edges ORDER BY from_path, kind, to_ref",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(EdgeRecord {
                from_path: r.get(0)?,
                kind: r.get(1)?,
                to_ref: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_edges(&mut self, edges: &[EdgeRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut del = tx.prepare_cached("DELETE FROM edges WHERE from_path = ?1")?;
            let mut cleared: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for e in edges {
                if cleared.insert(e.from_path.as_str()) {
                    del.execute(params![e.from_path])?;
                }
            }
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO edges (from_path, kind, to_ref) VALUES (?1, ?2, ?3)",
            )?;
            for e in edges {
                ins.execute(params![e.from_path, e.kind, e.to_ref])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All edges originating at `from_path` (the file's imports and the symbols it
    /// defines), ordered by kind then target for stable output.
    pub fn edges_from(&self, from_path: &str) -> Result<Vec<EdgeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_path, kind, to_ref FROM edges WHERE from_path = ?1 ORDER BY kind, to_ref",
        )?;
        let rows = stmt.query_map(params![from_path], |r| {
            Ok(EdgeRecord {
                from_path: r.get(0)?,
                kind: r.get(1)?,
                to_ref: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Reverse lookup: the distinct files that have a `kind` edge to `to_ref` — e.g. who
    /// imports a module (`kind="imports"`) or who defines a symbol (`kind="defines"`).
    /// Sorted for stable output.
    pub fn edges_to(&self, kind: &str, to_ref: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT from_path FROM edges WHERE kind = ?1 AND to_ref = ?2 ORDER BY from_path",
        )?;
        let rows = stmt.query_map(params![kind, to_ref], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The import strings recorded for one file, sorted. Used to build the tier-3
    /// (import-linked) matcher for that file.
    fn imports_of(&self, file: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT to_ref FROM edges WHERE from_path = ?1 AND kind = 'imports' ORDER BY to_ref",
        )?;
        let rows = stmt.query_map(params![file], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// D2 — files that contain a `calls` edge to `symbol` (direct callers), capped at
    /// `limit`. The match is on the bare symbol name, case-sensitive. See
    /// [`Self::who_calls_resolved`] for the tier-annotated version.
    pub fn who_calls(&self, symbol: &str, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT from_path FROM edges
              WHERE kind = 'calls' AND to_ref = ?1
              ORDER BY from_path LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![symbol, limit as i64], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// D2 (v0.25) — [`Self::who_calls`] with scoped resolution: each caller is
    /// annotated with how its call to `symbol` resolved (same-file / same-dir /
    /// import / bare) and, for resolved tiers, which definition file(s) it targets.
    /// Definition sites for `symbol`, narrowed to the user's answer when a
    /// `symbol_ambiguity` ledger question was decided with a specific path
    /// ("all" or no answer keeps the full set). This is what makes answering
    /// the question actually change graph behavior — the symbol-centric tools
    /// (`who_calls`, `blast_radius`) consult the pin; the scope-wide
    /// `code_graph` deliberately does not (one ledger lookup per distinct
    /// name in a scope would be a per-render tax).
    fn definers_with_pin(&self, symbol: &str) -> Result<(Vec<String>, bool)> {
        let definers = self.edges_to("defines", symbol)?;
        if let Some(d) = self.latest_decided("symbol_ambiguity", symbol)? {
            if let Some(chosen) = d.chosen.as_deref() {
                if chosen != "all" && definers.iter().any(|p| p == chosen) {
                    return Ok((vec![chosen.to_owned()], true));
                }
            }
        }
        Ok((definers, false))
    }

    pub fn who_calls_resolved(&self, symbol: &str, limit: usize) -> Result<Vec<ResolvedCaller>> {
        let callers = self.who_calls(symbol, limit)?;
        let defs = DefinerIndex::new(self.definers_with_pin(symbol)?.0);
        let mut out = Vec::with_capacity(callers.len());
        for caller in callers {
            // Tier 1: the caller's own definition wins outright — an intra-file helper
            // named like a popular symbol must not fan out repo-wide.
            if defs.set.contains(&caller) {
                out.push(ResolvedCaller {
                    targets: vec![caller.clone()],
                    path: caller,
                    tier: ResolutionTier::SameFile,
                });
                continue;
            }
            let it = import_targets(&caller, &self.imports_of(&caller)?);
            let (tier, targets) = resolve_call(&caller, &it, &defs);
            out.push(ResolvedCaller {
                path: caller,
                tier,
                // Bare targets would be the identical full definer list for every bare
                // caller — omitted; surfaces use `defines_count` instead.
                targets: if tier == ResolutionTier::Bare {
                    Vec::new()
                } else {
                    targets
                },
            });
        }
        Ok(out)
    }

    /// Files related to `path` through the call graph, ranked by the number of shared
    /// call→define symbols (the relation strength). Scoped-resolution wrapper kept for
    /// callers that don't need tiers — see [`Self::find_related_files_resolved`].
    pub fn find_related_files(&self, path: &str, limit: usize) -> Result<Vec<RelatedFile>> {
        Ok(self
            .find_related_files_resolved(path, limit)?
            .into_iter()
            .map(|r| RelatedFile {
                path: r.path,
                shared: r.shared,
            })
            .collect())
    }

    /// v0.25 — related files with scoped resolution. A file is related when `path`'s
    /// call **resolves** to it (dependency) or its call resolves to `path` (dependent);
    /// bare-name matches are kept as the fallback (labeled). A call to a symbol the
    /// calling file *itself* defines binds locally and relates nothing (this drops the
    /// cross-noise the old bare join produced). Over-common symbols (defined in more
    /// than [`Self::CODE_GRAPH_COMMON_SYMBOL_CAP`] files) are excluded as before.
    pub fn find_related_files_resolved(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<ResolvedRelatedFile>> {
        let cap = Self::CODE_GRAPH_COMMON_SYMBOL_CAP as usize;
        let mine = self.edges_from(path)?;
        let my_defines: HashSet<&str> = mine
            .iter()
            .filter(|e| e.kind == "defines")
            .map(|e| e.to_ref.as_str())
            .collect();
        let my_imports: Vec<String> = mine
            .iter()
            .filter(|e| e.kind == "imports")
            .map(|e| e.to_ref.clone())
            .collect();
        let it = import_targets(path, &my_imports);

        // path → (shared symbol count, best tier).
        let mut acc: HashMap<String, (usize, ResolutionTier)> = HashMap::new();
        let bump = |acc: &mut HashMap<String, (usize, ResolutionTier)>,
                    file: String,
                    tier: ResolutionTier| {
            let e = acc.entry(file).or_insert((0, tier));
            e.0 += 1;
            e.1 = e.1.min(tier);
        };
        let mut def_cache: HashMap<String, DefinerIndex> = HashMap::new();

        // Direction A: dependencies — files defining what `path` calls.
        for e in mine.iter().filter(|e| e.kind == "calls") {
            let s = e.to_ref.as_str();
            if my_defines.contains(s) {
                continue; // binds to the local definition
            }
            if !def_cache.contains_key(s) {
                def_cache.insert(
                    s.to_owned(),
                    DefinerIndex::new(self.edges_to("defines", s)?),
                );
            }
            let defs = &def_cache[s];
            if defs.all.is_empty() || defs.all.len() > cap {
                continue;
            }
            let (tier, targets) = resolve_call(path, &it, defs);
            for t in targets {
                bump(&mut acc, t, tier);
            }
        }

        // Direction B: dependents — files whose calls resolve to a symbol `path`
        // defines. Bare-name callers are kept (fallback); a caller whose call resolves
        // to a *different* definer is excluded (that was the old join's cross-noise).
        let pairs: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT c.from_path, c.to_ref FROM edges c
                  WHERE c.kind = 'calls' AND c.from_path <> ?1
                    AND c.to_ref IN (SELECT to_ref FROM edges WHERE kind = 'defines'
                                      AND from_path = ?1)
                  ORDER BY c.from_path, c.to_ref",
            )?;
            let rows = stmt.query_map(params![path], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut import_cache: HashMap<String, ImportTargets> = HashMap::new();
        for (f, s) in pairs {
            if !def_cache.contains_key(&s) {
                def_cache.insert(s.clone(), DefinerIndex::new(self.edges_to("defines", &s)?));
            }
            let defs = &def_cache[&s];
            if defs.all.len() > cap {
                continue;
            }
            if defs.set.contains(&f) {
                continue; // caller's own definition wins — not a dependent of `path`
            }
            if !import_cache.contains_key(&f) {
                let imports = self.imports_of(&f)?;
                import_cache.insert(f.clone(), import_targets(&f, &imports));
            }
            let (tier, targets) = resolve_call(&f, &import_cache[&f], defs);
            match tier {
                ResolutionTier::Bare => bump(&mut acc, f, ResolutionTier::Bare),
                _ if targets.iter().any(|t| t == path) => bump(&mut acc, f, tier),
                _ => {} // resolved to a different definer — cross-noise, dropped
            }
        }

        let mut out: Vec<ResolvedRelatedFile> = acc
            .into_iter()
            .map(|(path, (shared, tier))| ResolvedRelatedFile { path, shared, tier })
            .collect();
        out.sort_by(|a, b| b.shared.cmp(&a.shared).then_with(|| a.path.cmp(&b.path)));
        out.truncate(limit);
        Ok(out)
    }

    /// Detect dependency cycles in the file-to-file call graph under `prefix` (Tarjan SCC).
    /// Returns each strongly-connected component of size > 1 (a genuine cycle) as a sorted
    /// list of file paths, largest cycle first. Runs over the same edges as `code_graph`
    /// (scoped resolution; bare-name fallback edges can still create approximate cycles);
    /// `max_edges` bounds the graph it analyzes.
    pub fn find_cycles(&self, prefix: &str, max_edges: usize) -> Result<Vec<Vec<String>>> {
        let graph = self.code_graph(prefix, max_edges, false)?;
        // Index nodes; build adjacency (caller → callee).
        let nodes: Vec<&str> = graph.nodes.iter().map(|n| n.path.as_str()).collect();
        let idx: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for e in &graph.edges {
            if let (Some(&a), Some(&b)) = (idx.get(e.from.as_str()), idx.get(e.to.as_str())) {
                adj[a].push(b);
            }
        }
        let mut sccs = tarjan_scc(&adj);
        // Keep only true cycles (SCC size > 1), map indices back to paths, sort for stability.
        let mut cycles: Vec<Vec<String>> = sccs
            .drain(..)
            .filter(|c| c.len() > 1)
            .map(|c| {
                let mut paths: Vec<String> = c.into_iter().map(|i| nodes[i].to_owned()).collect();
                paths.sort();
                paths
            })
            .collect();
        cycles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        Ok(cycles)
    }

    /// How many distinct files `define` a symbol of this exact name. Used to **annotate**
    /// `who_calls` results: a name defined in >1 file is ambiguous, so bare-tier callers
    /// may conflate references to different definitions. `0` means the symbol isn't
    /// defined in the index.
    pub fn defines_count(&self, symbol: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT from_path) FROM edges WHERE kind = 'defines' AND to_ref = ?1",
            params![symbol],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// D2 — blast radius for `symbol` (compat wrapper over [`Self::blast_radius_resolved`]
    /// at the default depth of 2 = direct callers + one transitive hop; same files, no tier
    /// breakdown).
    pub fn blast_radius(&self, symbol: &str, limit: usize, strict: bool) -> Result<Vec<String>> {
        Ok(self.blast_radius_resolved(symbol, limit, strict, 2)?.files)
    }

    /// v0.25 — 1-hop blast radius with scoped resolution: direct callers of `symbol`
    /// **plus** files whose call to a direct caller's exported symbol *resolves to that
    /// caller* (same-dir or import tier). Bare-name transitive matches are kept as the
    /// labeled fallback; `strict` drops them entirely. A transitive candidate whose call
    /// resolves to a *different* definer (including its own file) is excluded — that was
    /// the old bare join's biggest false-positive source.
    ///
    /// The direct set itself is name-matched (the input is a bare name with no definer
    /// to disambiguate against) — see [`Self::who_calls_resolved`] for per-caller tiers.
    /// `depth` hops of caller reachability: `depth == 1` is direct callers only, `depth == 2`
    /// adds one transitive hop (the legacy default), and higher depths keep expanding from
    /// each newly-reached frontier of caller files. `included` doubles as the visited set, so
    /// cycles terminate; results are capped at `limit`.
    pub fn blast_radius_resolved(
        &self,
        symbol: &str,
        limit: usize,
        strict: bool,
        depth: usize,
    ) -> Result<BlastRadius> {
        let direct: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT from_path FROM edges
                  WHERE kind = 'calls' AND to_ref = ?1 ORDER BY from_path",
            )?;
            let rows = stmt.query_map(params![symbol], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        // A decided symbol_ambiguity answer pins the authoritative definition:
        // "what breaks if I change X?" then means the PINNED X, so direct
        // callers are kept only when their call can plausibly bind to it —
        // the pin itself, resolution targeting the pin, or bare-tier calls
        // (no evidence either way; strict drops those as everywhere else).
        // Callers defining the symbol themselves bind locally and are dropped.
        let (pin_vec, pinned) = self.definers_with_pin(symbol)?;
        let direct: Vec<String> = if pinned {
            let pin = pin_vec[0].clone();
            // Resolve against the FULL definer set: a caller whose import/dir
            // evidence points at a DIFFERENT definition is positively not a
            // caller of the pinned one — only the pin's own resolvers and
            // evidence-free (bare) callers remain.
            let full_defs = DefinerIndex::new(self.edges_to("defines", symbol)?);
            let mut kept = Vec::with_capacity(direct.len());
            for f in direct {
                if f == pin {
                    kept.push(f);
                    continue;
                }
                if full_defs.set.contains(&f) {
                    continue;
                }
                let it = import_targets(&f, &self.imports_of(&f)?);
                let (tier, targets) = resolve_call(&f, &it, &full_defs);
                let keep = match tier {
                    ResolutionTier::Bare => !strict,
                    _ => targets.contains(&pin),
                };
                if keep {
                    kept.push(f);
                }
            }
            kept
        } else {
            direct
        };
        // Resolution happens in Rust, so bound each hop's candidate set (deterministic order)
        // instead of letting a generic export (`new`) explode it.
        const TRANSITIVE_CANDIDATE_CAP: usize = 10_000;
        let mut included: BTreeSet<String> = direct.iter().cloned().collect();
        let mut def_cache: HashMap<String, DefinerIndex> = HashMap::new();
        let mut import_cache: HashMap<String, ImportTargets> = HashMap::new();
        let (mut scoped_transitive, mut bare_transitive) = (0usize, 0usize);

        // Hop 1 — the legacy transitive pass: files whose call to a direct caller's exported
        // symbol resolves back to a direct caller. Guarded by `depth >= 2` so `depth == 1`
        // returns direct callers only. `frontier` collects the files this hop adds, which seed
        // the next hop.
        let mut frontier: Vec<String> = Vec::new();
        if depth >= 2 {
            let direct_set: HashSet<&str> = direct.iter().map(String::as_str).collect();
            let candidates: Vec<(String, String)> = {
                let mut stmt = self.conn.prepare(
                    "WITH direct_callers AS (
                         SELECT DISTINCT from_path FROM edges
                          WHERE kind = 'calls' AND to_ref = ?1
                     ),
                     caller_exports AS (
                         SELECT DISTINCT to_ref FROM edges
                          WHERE kind = 'defines'
                            AND from_path IN (SELECT from_path FROM direct_callers)
                     )
                     SELECT DISTINCT from_path, to_ref FROM edges
                      WHERE kind = 'calls'
                        AND to_ref IN (SELECT to_ref FROM caller_exports)
                      ORDER BY from_path, to_ref
                      LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(params![symbol, TRANSITIVE_CANDIDATE_CAP as i64], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for (f, y) in candidates {
                if direct_set.contains(f.as_str()) || included.contains(&f) {
                    continue;
                }
                if let Some(scoped) = self.classify_transitive(
                    &f,
                    &y,
                    &direct_set,
                    strict,
                    &mut def_cache,
                    &mut import_cache,
                )? {
                    if scoped {
                        scoped_transitive += 1;
                    } else {
                        bare_transitive += 1;
                    }
                    included.insert(f.clone());
                    frontier.push(f);
                }
            }
        }

        // Hops 2..depth — keep expanding from the previous frontier of caller files. Each
        // candidate caller is kept only if its call resolves to a *frontier* file (scoped) or
        // is a bare-name fallback (dropped under `strict`). `included` is the visited set, so a
        // cycle (A→B→A) revisits nothing and the loop terminates.
        let mut hop = 2;
        while hop < depth && !frontier.is_empty() {
            let frontier_set: HashSet<&str> = frontier.iter().map(String::as_str).collect();
            let candidates = self.callers_of_exports(&frontier, TRANSITIVE_CANDIDATE_CAP)?;
            let mut next: Vec<String> = Vec::new();
            for (f, y) in candidates {
                if included.contains(&f) {
                    continue;
                }
                if let Some(scoped) = self.classify_transitive(
                    &f,
                    &y,
                    &frontier_set,
                    strict,
                    &mut def_cache,
                    &mut import_cache,
                )? {
                    if scoped {
                        scoped_transitive += 1;
                    } else {
                        bare_transitive += 1;
                    }
                    included.insert(f.clone());
                    next.push(f);
                }
            }
            frontier = next;
            hop += 1;
        }

        Ok(BlastRadius {
            files: included.into_iter().take(limit).collect(),
            direct: direct.len(),
            scoped_transitive,
            bare_transitive,
        })
    }

    /// Classify whether candidate caller `f` (which calls exported symbol `y`) links into the
    /// current `frontier_set`: `Some(true)` if its call resolves (same-dir/import) to a frontier
    /// file, `Some(false)` if it's kept only as a bare-name fallback, `None` if dropped (binds to
    /// its own definition, resolves to a non-frontier definer, or bare under `strict`). Fills the
    /// per-symbol definer cache and per-file import cache so repeated hops stay cheap.
    #[allow(clippy::too_many_arguments)]
    fn classify_transitive(
        &self,
        f: &str,
        y: &str,
        frontier_set: &HashSet<&str>,
        strict: bool,
        def_cache: &mut HashMap<String, DefinerIndex>,
        import_cache: &mut HashMap<String, ImportTargets>,
    ) -> Result<Option<bool>> {
        if !def_cache.contains_key(y) {
            def_cache.insert(
                y.to_string(),
                DefinerIndex::new(self.edges_to("defines", y)?),
            );
        }
        let defs = &def_cache[y];
        if defs.set.contains(f) {
            return Ok(None); // f's call binds to its own definition — not a link here
        }
        if !import_cache.contains_key(f) {
            let imports = self.imports_of(f)?;
            import_cache.insert(f.to_string(), import_targets(f, &imports));
        }
        let (tier, targets) = resolve_call(f, &import_cache[f], defs);
        Ok(match tier {
            ResolutionTier::Bare if !strict => Some(false),
            ResolutionTier::Bare => None,
            _ if targets.iter().any(|t| frontier_set.contains(t.as_str())) => Some(true),
            _ => None, // resolved to a non-frontier definer — cross-noise, dropped
        })
    }

    /// Candidate `(caller, exported-symbol)` pairs for the next reachability hop: every file
    /// that calls a symbol *exported by* one of the `frontier` files. Bounded by `cap` in a
    /// deterministic order. Returns empty when `frontier` is empty.
    fn callers_of_exports(&self, frontier: &[String], cap: usize) -> Result<Vec<(String, String)>> {
        if frontier.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; frontier.len()].join(",");
        let sql = format!(
            "WITH caller_exports AS (
                 SELECT DISTINCT to_ref FROM edges
                  WHERE kind = 'defines' AND from_path IN ({placeholders})
             )
             SELECT DISTINCT from_path, to_ref FROM edges
              WHERE kind = 'calls'
                AND to_ref IN (SELECT to_ref FROM caller_exports)
              ORDER BY from_path, to_ref
              LIMIT ?"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let cap_i = cap as i64;
        let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(frontier.len() + 1);
        for f in frontier {
            binds.push(f);
        }
        binds.push(&cap_i);
        let rows = stmt.query_map(binds.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Build a file-to-file **call graph** for files under `prefix` (compat wrapper
    /// over [`Self::code_graph_scoped`]; same edges, no tier annotations).
    pub fn code_graph(&self, prefix: &str, max_edges: usize, strict: bool) -> Result<CodeGraph> {
        Ok(self.code_graph_scoped(prefix, max_edges, strict)?.graph)
    }

    /// v0.25 — the signature graph with **scoped call resolution**.
    ///
    /// For each `calls X` edge from file A, X's definition sites are ranked:
    /// 1. **same-file** — A defines X itself: the call binds locally, producing **no
    ///    cross-file edge** (an intra-file helper named like a popular symbol no
    ///    longer fans out repo-wide).
    /// 2. **same-dir** — definers sharing A's directory: only those are linked.
    /// 3. **import** — definers whose path matches one of A's import strings (see the
    ///    matcher above for exactly which forms resolve).
    /// 4. **bare** — all remaining definers (the historical behavior), labeled.
    ///
    /// The edge universe is the same as before — scoped resolution only *narrows* it
    /// (never adds edges), so PageRank / Map sizing stay comparable. Symbols defined in
    /// more than [`Self::CODE_GRAPH_COMMON_SYMBOL_CAP`] files (whole-index count, as
    /// before) are excluded as generic-name noise. Both endpoints must be under
    /// `prefix`; definers outside the scope are not considered. `strict` drops the
    /// bare tier entirely — only structurally-resolved edges remain (it used to be a
    /// unique-definition name filter; resolution supersedes that).
    ///
    /// Resolution needs per-caller context (own defines + imports) that a single SQL
    /// join can't express, so the scope's edges are loaded and resolved in memory.
    pub fn code_graph_scoped(
        &self,
        prefix: &str,
        max_edges: usize,
        strict: bool,
    ) -> Result<ScopedCodeGraph> {
        // Normalize to a directory prefix so `/a/proj` doesn't also match `/a/projector`.
        // `/` (whole disk) is left as-is → matches everything.
        let dir = if prefix == "/" || prefix.ends_with('/') {
            prefix.to_owned()
        } else {
            format!("{prefix}/")
        };
        let pattern = like_prefix(&dir);

        // One pass over the scope's edges → per-caller and per-symbol context.
        let mut calls: Vec<(String, String)> = Vec::new();
        let mut defines_by_symbol: HashMap<String, Vec<String>> = HashMap::new();
        let mut defines_by_file: HashMap<String, HashSet<String>> = HashMap::new();
        let mut imports_by_file: HashMap<String, Vec<String>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT from_path, kind, to_ref FROM edges
                  WHERE from_path LIKE ?1 ESCAPE '\\'
                    AND kind IN ('calls', 'defines', 'imports')
                  ORDER BY from_path, to_ref",
            )?;
            let rows = stmt.query_map(params![pattern], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (from, kind, to) = row?;
                match kind.as_str() {
                    "calls" => calls.push((from, to)),
                    "defines" => {
                        defines_by_symbol
                            .entry(to.clone())
                            .or_default()
                            .push(from.clone());
                        defines_by_file.entry(from).or_default().insert(to);
                    }
                    _ => imports_by_file.entry(from).or_default().push(to),
                }
            }
        }

        // Whole-index definer counts drive the generic-name cap, exactly like the old
        // SQL subquery — `new`/`from`/`default` stay excluded even when the scope only
        // sees a few of their definers.
        let over_common: HashSet<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT to_ref FROM edges WHERE kind = 'defines'
                  GROUP BY to_ref HAVING COUNT(DISTINCT from_path) > ?1",
            )?;
            let rows = stmt.query_map(params![Self::CODE_GRAPH_COMMON_SYMBOL_CAP], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };

        // Lazy per-symbol definer indexes and per-caller import matchers.
        let mut def_idx: HashMap<String, DefinerIndex> = HashMap::new();
        let mut import_cache: HashMap<String, ImportTargets> = HashMap::new();

        // (caller, callee) → (distinct shared symbols, best tier).
        let mut acc: HashMap<(String, String), (usize, ResolutionTier)> = HashMap::new();
        for (caller, symbol) in &calls {
            if over_common.contains(symbol) || !defines_by_symbol.contains_key(symbol) {
                continue;
            }
            // Tier 1: the caller's own definition wins — self-edge, never displayed.
            if defines_by_file
                .get(caller)
                .is_some_and(|s| s.contains(symbol))
            {
                continue;
            }
            if !def_idx.contains_key(symbol) {
                def_idx.insert(
                    symbol.clone(),
                    DefinerIndex::new(defines_by_symbol[symbol].clone()),
                );
            }
            if !import_cache.contains_key(caller) {
                let empty = Vec::new();
                let imports = imports_by_file.get(caller).unwrap_or(&empty);
                import_cache.insert(caller.clone(), import_targets(caller, imports));
            }
            let (tier, targets) = resolve_call(caller, &import_cache[caller], &def_idx[symbol]);
            if strict && tier == ResolutionTier::Bare {
                continue;
            }
            for t in targets {
                let e = acc.entry((caller.clone(), t)).or_insert((0, tier));
                e.0 += 1;
                e.1 = e.1.min(tier);
            }
        }

        // Same ordering contract as the old SQL: weight DESC, caller, callee.
        let mut raw: Vec<(String, String, usize, ResolutionTier)> = acc
            .into_iter()
            .map(|((from, to), (w, tier))| (from, to, w, tier))
            .collect();
        raw.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        let truncated = raw.len() > max_edges;
        raw.truncate(max_edges);

        // Accumulate degree counts per node.
        let mut out_deg: HashMap<&str, usize> = HashMap::new();
        let mut in_deg: HashMap<&str, usize> = HashMap::new();
        let mut edges = Vec::with_capacity(raw.len());
        let mut edge_tiers = Vec::with_capacity(raw.len());
        for (from, to, weight, tier) in &raw {
            *out_deg.entry(from.as_str()).or_insert(0) += 1;
            *in_deg.entry(to.as_str()).or_insert(0) += 1;
            edges.push(CodeGraphEdge {
                from: from.clone(),
                to: to.clone(),
                weight: *weight,
            });
            edge_tiers.push(*tier);
        }

        // Node set = every path that appears as a caller or callee (sorted for
        // stable ordering, then indexed so PageRank can run on integer ids).
        let mut paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (from, to, _, _) in &raw {
            paths.insert(from.as_str());
            paths.insert(to.as_str());
        }
        let node_paths: Vec<&str> = paths.into_iter().collect();
        let idx: HashMap<&str, usize> = node_paths
            .iter()
            .enumerate()
            .map(|(i, &p)| (p, i))
            .collect();

        // Weighted PageRank over the displayed (post-cap) edge set: rank flows
        // caller → callee, so hub files called by many score highest.
        let pr_edges: Vec<(usize, usize, f64)> = raw
            .iter()
            .map(|(from, to, weight, _)| (idx[from.as_str()], idx[to.as_str()], *weight as f64))
            .collect();
        let scores = super::pagerank::pagerank(node_paths.len(), &pr_edges);

        let nodes = node_paths
            .iter()
            .enumerate()
            .map(|(i, &p)| CodeGraphNode {
                path: p.to_owned(),
                out_degree: out_deg.get(p).copied().unwrap_or(0),
                in_degree: in_deg.get(p).copied().unwrap_or(0),
                pagerank: scores[i],
            })
            .collect();

        Ok(ScopedCodeGraph {
            graph: CodeGraph {
                nodes,
                edges,
                truncated,
            },
            edge_tiers,
        })
    }
}
