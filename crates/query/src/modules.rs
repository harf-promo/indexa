//! Architecture-map labeling (4.6): the LLM-touching half of the persisted-modules feature.
//! `indexa_core::store::cluster_with_directory_priors` does the pure clustering; this module
//! adds the local-LLM label per cluster (from member L0 abstracts) and persists the result.
//! Lives here, not in `indexa-core`, because labeling needs [`Generator`] — the same split
//! `qa/cluster.rs`'s `graphrag_summarize` pass already uses for per-cluster theme summaries.

use indexa_core::store::{cluster_with_directory_priors, ComputedModule, Store};
use indexa_llm::Generator;

/// Chars of joined L0 abstracts fed into one label call — mirrors `qa/cluster.rs`'s
/// `CLUSTER_SUMMARY_INPUT_BUDGET`, bounding cost regardless of cluster size.
const LABEL_INPUT_BUDGET: usize = 1200;

/// Max label characters accepted from the LLM before falling back — a runaway/rambling
/// response is dropped rather than persisted verbatim.
const MAX_LABEL_LEN: usize = 80;

/// How many of the largest clusters get an LLM label call, GitNexus-style: bounded between 20
/// and 300 regardless of repo size, scaling with node count in between. The rest (smaller,
/// less central clusters) get a cheap deterministic fallback label — full membership is still
/// persisted for all of them, only the label text differs.
pub fn label_cap(node_count: usize) -> usize {
    (node_count / 10).clamp(20, 300)
}

/// Tight prompt for naming a cluster of related files from their L0 abstracts — the module
/// sibling of `qa/cluster.rs`'s `cluster_theme_prompt`.
fn module_label_prompt(joined_abstracts: &str) -> String {
    format!(
        "In ONE short phrase (≤6 words), name the functional area these related files belong \
         to in a codebase, based on their one-line summaries below. Output only the phrase, no \
         preamble or punctuation.\n\n{joined_abstracts}\n\nAREA:"
    )
}

/// Deterministic fallback label when a cluster isn't LLM-labeled (beyond `label_cap`) or the LLM
/// call fails/returns something unusable — never blocks persistence on a model being available.
fn fallback_label(members: &[String]) -> String {
    let dir = members
        .first()
        .and_then(|p| std::path::Path::new(p).parent())
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if members.len() == 1 {
        std::path::Path::new(&members[0])
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| members[0].clone())
    } else if dir.is_empty() {
        format!("{} files", members.len())
    } else {
        format!("{} files near {dir}", members.len())
    }
}

/// Label one cluster: skip the LLM entirely for a singleton (nothing to summarize — mirrors
/// `graphrag_summarize`'s "skip singletons" precedent), otherwise gather member L0 abstracts (each
/// read via `Store::summary_by_path`, missing/unsummarized members silently skipped — fail-open)
/// up to `LABEL_INPUT_BUDGET` chars and ask the LLM for a short phrase. Any failure — no
/// summaries at all, an LLM error, an empty or oversized response — falls back to
/// [`fallback_label`] rather than leaving the cluster unlabeled.
async fn label_cluster(store: &Store, llm: &dyn Generator, members: &[String]) -> String {
    if members.len() < 2 {
        return fallback_label(members);
    }
    let mut joined = String::new();
    for path in members {
        if joined.len() >= LABEL_INPUT_BUDGET {
            break;
        }
        let Ok(Some(summary)) = store.summary_by_path(path) else {
            continue;
        };
        let abstract_line = summary
            .summary_l0
            .unwrap_or_else(|| indexa_core::store::abstract_from(&summary.summary));
        if abstract_line.is_empty() {
            continue;
        }
        let take = LABEL_INPUT_BUDGET - joined.len();
        joined.push_str(&abstract_line.chars().take(take).collect::<String>());
        joined.push('\n');
    }
    if joined.trim().is_empty() {
        return fallback_label(members);
    }
    match llm.generate(&module_label_prompt(&joined)).await {
        Ok(label) => {
            let label = label.trim();
            if label.is_empty() || label.len() > MAX_LABEL_LEN {
                fallback_label(members)
            } else {
                label.to_owned()
            }
        }
        Err(_) => fallback_label(members),
    }
}

/// Recompute the whole-repo architecture map under `scope`: cluster via
/// `cluster_with_directory_priors`, label the largest `label_cap` clusters with `llm`, fallback-
/// label the rest, and persist wholesale via `Store::replace_graph_modules`. Returns the number
/// of modules written (0 when the scope has no code graph — the table is cleared either way, so
/// a stale prior computation is never left mixed with an unrelated new one).
pub async fn recompute_graph_modules(
    store: &mut Store,
    llm: &dyn Generator,
    scope: &str,
    max_edges: usize,
) -> anyhow::Result<usize> {
    let scoped = store.code_graph_scoped(scope, max_edges, false)?;
    let mut modules: Vec<ComputedModule> = cluster_with_directory_priors(&scoped.graph);
    if modules.is_empty() {
        store.replace_graph_modules(&[])?;
        return Ok(0);
    }

    let cap = label_cap(scoped.graph.nodes.len());
    for (i, m) in modules.iter_mut().enumerate() {
        m.label = if i < cap {
            label_cluster(store, llm, &m.members).await
        } else {
            fallback_label(&m.members)
        };
    }

    store.replace_graph_modules(&modules)?;
    Ok(modules.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeLlm(Result<&'static str, ()>);

    #[async_trait::async_trait]
    impl Generator for FakeLlm {
        async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
            self.0
                .map(|s| s.to_owned())
                .map_err(|_| anyhow::anyhow!("fake failure"))
        }
    }

    #[test]
    fn label_cap_is_clamped_between_20_and_300() {
        assert_eq!(label_cap(0), 20);
        assert_eq!(label_cap(100), 20);
        assert_eq!(label_cap(1000), 100);
        assert_eq!(label_cap(10_000), 300);
        assert_eq!(label_cap(100_000), 300);
    }

    #[test]
    fn fallback_label_names_a_singleton_by_its_filename() {
        assert_eq!(fallback_label(&["/repo/a/one.rs".to_owned()]), "one.rs");
    }

    #[test]
    fn fallback_label_names_a_group_by_size_and_directory() {
        let label = fallback_label(&["/repo/a/one.rs".to_owned(), "/repo/a/two.rs".to_owned()]);
        assert!(label.contains('2'));
        assert!(label.contains("/repo/a"));
    }

    #[tokio::test]
    async fn label_cluster_skips_the_llm_for_a_singleton() {
        let store = Store::open_in_memory().unwrap();
        let llm = FakeLlm(Err(()));
        let label = label_cluster(&store, &llm, &["/a.rs".to_owned()]).await;
        assert_eq!(label, "a.rs");
    }

    #[tokio::test]
    async fn label_cluster_falls_back_when_no_summaries_exist() {
        let store = Store::open_in_memory().unwrap();
        let llm = FakeLlm(Ok("Auth Layer"));
        // Two members but neither has a summary row — nothing to feed the LLM.
        let label = label_cluster(&store, &llm, &["/a.rs".to_owned(), "/b.rs".to_owned()]).await;
        assert!(label.contains('2'));
    }

    #[tokio::test]
    async fn label_cluster_falls_back_on_an_oversized_llm_response() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_summary(&indexa_core::store::SummaryRecord {
                path: "/a.rs".to_owned(),
                kind: "file".to_owned(),
                parent_path: None,
                depth: 0,
                summary: "Handles authentication.".to_owned(),
                summary_l0: None,
                embedding: None,
                child_count: 0,
                byte_size: 10,
                model: "test".to_owned(),
                source_hash: "h".to_owned(),
                generated_at: 0,
            })
            .unwrap();
        let long: &'static str = "this response rambles on for far too long to be a real label and should be rejected outright by the length guard";
        let llm = FakeLlm(Ok(long));
        let label = label_cluster(&store, &llm, &["/a.rs".to_owned(), "/b.rs".to_owned()]).await;
        assert_ne!(label, long);
    }

    #[tokio::test]
    async fn recompute_graph_modules_clears_the_table_when_the_scope_has_no_graph() {
        let mut store = Store::open_in_memory().unwrap();
        let llm = FakeLlm(Ok("x"));
        let n = recompute_graph_modules(&mut store, &llm, "/nowhere", 200)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(store.graph_modules().unwrap().is_empty());
    }
}
