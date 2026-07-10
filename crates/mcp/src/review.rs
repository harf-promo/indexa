//! Decision review tools: the Decision Ledger's MCP surface (v0.22) — open
//! questions, durable answers, sticky dismissal, and the revision chain.
//!
//! Answers route through `decisions::decide_and_apply` like every other
//! surface: the ledger row commits first (provenance is never lost), then the
//! idempotent projection updates the domain tables.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router, ErrorData,
};
use serde::Deserialize;

use indexa_core::decisions::{self, templates::render_question, DecisionType};
use indexa_core::store::DecisionRecord;

use crate::{mcp_err, ok_text, IndexaMcp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListOpenDecisionsParams {
    /// Filter by decision type: `classification` or `duplicate`. Omit for all.
    #[serde(default)]
    pub decision_type: Option<String>,
    /// Max questions to return (default 50).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Skip the first N questions — page through a long inbox by advancing
    /// `offset` by `limit` each call (default 0).
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetDecisionParams {
    /// Decision id (from `list_open_decisions` or `decision_history`).
    pub id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnswerDecisionParams {
    /// Decision id (from `list_open_decisions`).
    pub id: i64,
    /// One of the question's option values (the part before ` — ` in each
    /// option line).
    pub chosen: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DismissDecisionParams {
    /// Decision id (from `list_open_decisions`).
    pub id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DecisionHistoryParams {
    /// The subject the decisions are about — a path or duplicate-cluster key,
    /// exactly as shown by the other decision tools.
    pub subject: String,
}

/// One rendered question as agents see it: id, title, detail, then each option
/// as a `value — label` line (the value is what `answer_decision` expects).
fn format_question(q: &decisions::templates::RenderedQuestion) -> String {
    let mut s = format!(
        "#{} [{}] {}\n   {}\n   Options:\n",
        q.id, q.decision_type, q.title, q.detail
    );
    for (value, label) in &q.options {
        s.push_str(&format!("     {value} — {label}\n"));
    }
    s
}

/// One revision-chain line: outcome plus the chain markers that explain why a
/// row exists (`re-ask of`) and why it stopped being current (`superseded by`).
fn history_line(d: &DecisionRecord) -> String {
    let outcome = match d.status.as_str() {
        "open" => "open — awaiting an answer".to_owned(),
        "decided" => format!(
            "decided — chose \"{}\" ({})",
            d.chosen.as_deref().unwrap_or("?"),
            d.source.as_deref().unwrap_or("?")
        ),
        "dismissed" => "dismissed".to_owned(),
        "expired" => "expired".to_owned(),
        other => other.to_owned(),
    };
    let mut markers = String::new();
    if let Some(p) = d.parent_id {
        markers.push_str(&format!(" (re-ask of #{p})"));
    }
    if let Some(s) = d.superseded_by {
        markers.push_str(&format!(" (superseded by #{s})"));
    }
    format!("#{} [{}] {outcome}{markers}", d.id, d.decision_type)
}

#[tool_router(router = router_review, vis = "pub(crate)")]
impl IndexaMcp {
    /// List open Decision Ledger questions in inbox order.
    #[tool(
        description = "List open Decision Ledger questions — judgment calls indexing was not \
                       confident enough to make alone (uncertain folder classifications, \
                       duplicate clusters needing a canonical copy). These are questions Indexa \
                       needs a human judgment on. You may relay a question to your user and \
                       answer on their behalf with answer_decision. Filter by decision_type: \
                       `classification` or `duplicate`.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn list_open_decisions(
        &self,
        params: Parameters<ListOpenDecisionsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let ListOpenDecisionsParams {
            decision_type,
            limit,
            offset,
        } = params.0;
        if let Some(t) = decision_type.as_deref() {
            if DecisionType::parse(t).is_none() {
                return Err(mcp_err(format!(
                    "unknown decision type '{t}' (valid: {})",
                    DecisionType::ALL.map(DecisionType::as_str).join(", ")
                )));
            }
        }
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);
        let store = self.store()?;
        let rows = store
            .open_decisions_paged(decision_type.as_deref(), limit, offset)
            .map_err(mcp_err)?;
        if rows.is_empty() {
            let cleared = if offset > 0 {
                format!("No more open questions past offset {offset}.")
            } else {
                "No open questions — the Decision Ledger is clear.".to_owned()
            };
            return Ok(ok_text(cleared));
        }
        // Number rows by absolute inbox position so a paged listing stays unambiguous.
        let blocks: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, d)| {
                format!(
                    "{}. {}",
                    offset + i + 1,
                    format_question(&render_question(d))
                )
            })
            .collect();
        // Hint the next page only when this one came back full (more may remain).
        let more = if rows.len() == limit {
            format!(
                "\n\n(More may remain — call again with offset: {}.)",
                offset + limit
            )
        } else {
            String::new()
        };
        Ok(ok_text(format!(
            "{} open question(s) — answer with answer_decision(id, chosen):\n\n{}{more}",
            rows.len(),
            blocks.join("\n")
        )))
    }

    /// Show one decision in full: question, status, and chain links.
    #[tool(
        description = "Show one Decision Ledger question in full: rendered title/detail, the \
                       options (answer with one of the `value` strings), current status, applied \
                       effects, and its revision-chain links (re-ask parent / superseding \
                       revision).",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn get_decision(
        &self,
        params: Parameters<GetDecisionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let GetDecisionParams { id } = params.0;
        let store = self.store()?;
        let d = store
            .decision_by_id(id)
            .map_err(mcp_err)?
            .ok_or_else(|| mcp_err(format!("no decision with id {id}")))?;
        let mut out = format_question(&render_question(&d));
        out.push_str(&match d.status.as_str() {
            "open" => "Status: open — awaiting an answer.\n".to_owned(),
            "decided" => format!(
                "Status: decided — chose \"{}\" (source: {}).\n",
                d.chosen.as_deref().unwrap_or("?"),
                d.source.as_deref().unwrap_or("?")
            ),
            "dismissed" => "Status: dismissed — returns only if the evidence changes.\n".to_owned(),
            "expired" => "Status: expired — the subject vanished from the index.\n".to_owned(),
            other => format!("Status: {other}.\n"),
        });
        if let Some(effects) = &d.effects {
            out.push_str(&format!("Effects: {effects}\n"));
        }
        let mut chain = Vec::new();
        if let Some(p) = d.parent_id {
            chain.push(format!("re-ask of #{p}"));
        }
        if let Some(s) = d.superseded_by {
            chain.push(format!("superseded by #{s}"));
        }
        if !chain.is_empty() {
            out.push_str(&format!("Chain: {}.\n", chain.join("; ")));
        }
        Ok(ok_text(out))
    }

    /// Answer an open question; records provenance and applies the effects.
    #[tool(
        description = "Answer an open Decision Ledger question with one of its option values. \
                       The answer is recorded durably with provenance (source: user) and applies \
                       immediately — e.g. confirming a classification writes the category; \
                       picking a duplicate canonical down-weights the other copies to 0 in \
                       search.",
        annotations(destructive_hint = true)
    )]
    pub(crate) async fn answer_decision(
        &self,
        params: Parameters<AnswerDecisionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let AnswerDecisionParams { id, chosen } = params.0;
        let mut store = self.store()?;
        // decide_and_apply validates `chosen` against the row's options BEFORE
        // committing, then projects the answer onto the domain tables.
        let effects =
            decisions::decide_and_apply(&mut store, id, &chosen, "user").map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Recorded \"{chosen}\" for decision #{id} (source: user). Effects applied: {effects}"
        )))
    }

    /// Dismiss an open question; sticky for the same evidence.
    #[tool(
        description = "Dismiss an open Decision Ledger question without answering it. Sticky \
                       for the same evidence: the question returns only if the evidence behind \
                       it changes (e.g. the folder's contents shift).",
        annotations(destructive_hint = true)
    )]
    pub(crate) async fn dismiss_decision(
        &self,
        params: Parameters<DismissDecisionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let DismissDecisionParams { id } = params.0;
        let mut store = self.store()?;
        store.dismiss_decision(id).map_err(mcp_err)?;
        Ok(ok_text(format!(
            "Decision #{id} dismissed. It will not be asked again unless the evidence behind \
             it changes."
        )))
    }

    /// The revision chain recorded for a subject, oldest first.
    #[tool(
        description = "Show every Decision Ledger revision recorded for a subject (a path or \
                       duplicate-cluster key), chronological, with status and superseded \
                       markers — the audit trail of how an answer evolved.",
        annotations(read_only_hint = true)
    )]
    pub(crate) async fn decision_history(
        &self,
        params: Parameters<DecisionHistoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let DecisionHistoryParams { subject } = params.0;
        let store = self.store()?;
        // A subject is keyed per type in the store; merge across all types so
        // the tool answers for "this path" without the agent knowing the type.
        // Subjects are stored canonicalized — fall back to the canonical form
        // so a symlinked path (macOS /var → /private/var) still finds its
        // history (mirrors the CLI's `review history`).
        let mut rows: Vec<DecisionRecord> = Vec::new();
        for t in DecisionType::ALL {
            rows.extend(
                store
                    .decision_history(t.as_str(), &subject)
                    .map_err(mcp_err)?,
            );
        }
        if rows.is_empty() {
            if let Ok(canon) = std::fs::canonicalize(&subject) {
                let canon = canon.to_string_lossy();
                if canon != subject {
                    for t in DecisionType::ALL {
                        rows.extend(
                            store
                                .decision_history(t.as_str(), &canon)
                                .map_err(mcp_err)?,
                        );
                    }
                }
            }
        }
        rows.sort_by_key(|d| d.id);
        if rows.is_empty() {
            return Ok(ok_text(format!("No decisions recorded for \"{subject}\".")));
        }
        let lines: Vec<String> = rows.iter().map(history_line).collect();
        Ok(ok_text(format!(
            "{} revision(s) for \"{subject}\" (oldest first):\n\n{}",
            rows.len(),
            lines.join("\n")
        )))
    }
}
