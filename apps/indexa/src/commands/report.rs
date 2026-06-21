use anyhow::{Context, Result};
use indexa_core::config::Config;
use indexa_core::store::Store;
use indexa_query::{answer, Answer, QaConfig};

use super::helpers::{build_embedder, build_llm, now_unix, require_index_db};

/// `indexa report` — run several questions and render one document (Markdown or XML)
/// with a table of contents, each answer, and its cited sources. An "onboarding /
/// design-doc generator" built on the same `ask` pipeline.
pub(crate) async fn cmd_report(
    questions: Vec<String>,
    saved: Vec<String>,
    format: String,
    output: Option<String>,
    cfg: &Config,
) -> Result<()> {
    let Some(db_path) = require_index_db()? else {
        return Ok(());
    };
    let store = Store::open(&db_path)?;
    if store.chunk_count()? == 0 {
        println!("No deep-scanned content found. Run `indexa index <path>` first.");
        return Ok(());
    }

    // Assemble the question list: explicit ones first, then any saved queries by name.
    let mut qs: Vec<String> = questions;
    for name in &saved {
        match store.get_saved_query(name)? {
            Some(q) => qs.push(q.question),
            None => eprintln!("  ⚠  no saved query named \"{name}\" — skipping."),
        }
    }
    if qs.is_empty() {
        anyhow::bail!("no questions given. Pass questions and/or --saved <name>.");
    }
    drop(store);

    let embedder = build_embedder(cfg, None)?;
    let llm = build_llm(cfg, None)?;
    let qa_cfg = QaConfig {
        top_k: cfg.retrieval.top_k,
        mode: cfg.retrieval.hybrid.clone(),
        scope: None,
        context_budget: cfg.retrieval.context_budget,
        rrf_k: cfg.retrieval.rrf_k as f32,
        summary_weight: cfg.retrieval.summary_weight,
        summary_depth_alpha: cfg.retrieval.summary_depth_alpha,
        rerank: cfg.retrieval.rerank,
        rerank_backend: cfg.retrieval.rerank_backend.clone(),
        use_weights: cfg.retrieval.use_weights,
        use_recency_weight: cfg.retrieval.recency_boost,
        recency_days: cfg.retrieval.recency_days,
        max_steps: cfg.retrieval.agentic_max_steps,
        mmr_lambda: cfg.retrieval.mmr_lambda,
        archive_segments: cfg.retrieval.archive_segments.clone(),
        archive_penalty: cfg.retrieval.archive_penalty,
        broad_per_file_cap: cfg.retrieval.broad_per_file_cap,
        graphrag_clusters: cfg.retrieval.graphrag_clusters,
        graphrag_max_clusters: cfg.retrieval.graphrag_max_clusters,
        graphrag_cluster_sim: cfg.retrieval.graphrag_cluster_sim,
        graphrag_summarize: cfg.retrieval.graphrag_summarize,
    };

    let mut answers: Vec<Answer> = Vec::with_capacity(qs.len());
    for (i, q) in qs.iter().enumerate() {
        eprintln!("  [{}/{}] {q}", i + 1, qs.len());
        let a = answer(&db_path, embedder.as_ref(), llm.as_ref(), q, &qa_cfg).await?;
        answers.push(a);
    }

    let now = now_unix().to_string();
    let doc = if format == "xml" {
        render_report_xml(&answers, &now)
    } else {
        render_report_md(&answers, &now)
    };

    if let Some(path) = output {
        std::fs::write(&path, &doc).with_context(|| format!("writing report to '{path}'"))?;
        eprintln!("Wrote {} bytes to {path}.", doc.len());
    } else {
        print!("{doc}");
    }
    Ok(())
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn render_report_md(answers: &[Answer], generated_at: &str) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("# Indexa report\n\n");
    out.push_str(&format!(
        "_generated {generated_at} · {} quer{}_\n\n",
        answers.len(),
        if answers.len() == 1 { "y" } else { "ies" }
    ));
    out.push_str("## Contents\n\n");
    for (i, a) in answers.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}](#{}-{})\n",
            i + 1,
            a.question,
            i + 1,
            slug(&a.question)
        ));
    }
    out.push('\n');
    for (i, a) in answers.iter().enumerate() {
        out.push_str(&format!("## {}. {}\n\n", i + 1, a.question));
        out.push_str(&format!("{}\n\n", a.answer));
        if !a.sources.is_empty() {
            out.push_str("**Sources:**\n\n");
            for s in &a.sources {
                if s.heading.is_empty() {
                    out.push_str(&format!("- `{}`\n", s.path));
                } else {
                    out.push_str(&format!("- `{}` — {}\n", s.path, s.heading));
                }
            }
            out.push('\n');
        }
    }
    out
}

fn render_report_xml(answers: &[Answer], generated_at: &str) -> String {
    // XML escaping centralized in indexa_core::text. The 4-char attribute escaper is used
    // for both attributes and the <answer> text below — escaping `"` in text is harmless.
    let esc = indexa_core::text::xml_escape_attr;
    let mut out = String::with_capacity(4096);
    out.push_str(&format!(
        "<report generated_at=\"{generated_at}\" queries=\"{}\">\n",
        answers.len()
    ));
    for (i, a) in answers.iter().enumerate() {
        out.push_str(&format!(
            "  <query n=\"{}\" question=\"{}\">\n",
            i + 1,
            esc(&a.question)
        ));
        out.push_str(&format!("    <answer>{}</answer>\n", esc(&a.answer)));
        if !a.sources.is_empty() {
            out.push_str("    <sources>\n");
            for s in &a.sources {
                out.push_str(&format!(
                    "      <source path=\"{}\" heading=\"{}\"/>\n",
                    esc(&s.path),
                    esc(&s.heading)
                ));
            }
            out.push_str("    </sources>\n");
        }
        out.push_str("  </query>\n");
    }
    out.push_str("</report>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexa_query::SourceCitation;

    fn sample() -> Vec<Answer> {
        vec![Answer {
            question: "How does auth work?".to_owned(),
            answer: "It uses tokens & sessions.".to_owned(),
            sources: vec![SourceCitation {
                path: "/src/auth.rs".to_owned(),
                heading: "login".to_owned(),
                snippet: "…".to_owned(),
            }],
            confidence: None,
            synthesized: true,
            model: None,
        }]
    }

    #[test]
    fn md_report_has_toc_answer_and_sources() {
        let md = render_report_md(&sample(), "123");
        assert!(md.starts_with("# Indexa report"));
        assert!(md.contains("## Contents"));
        assert!(md.contains("## 1. How does auth work?"));
        assert!(md.contains("It uses tokens & sessions."));
        assert!(md.contains("`/src/auth.rs` — login"));
    }

    #[test]
    fn xml_report_escapes_and_structures() {
        let xml = render_report_xml(&sample(), "123");
        assert!(xml.starts_with("<report "));
        assert!(xml.contains("queries=\"1\""));
        assert!(xml.contains("<answer>It uses tokens &amp; sessions.</answer>"));
        assert!(xml.contains("<source path=\"/src/auth.rs\" heading=\"login\"/>"));
        assert!(xml.trim_end().ends_with("</report>"));
    }
}
