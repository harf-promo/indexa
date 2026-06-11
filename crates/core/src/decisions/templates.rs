//! Late rendering: structured ledger params → a displayable question.
//!
//! Rows store evidence (params/options), never prose — so wording can improve
//! without migrating data. CLI and MCP render through [`render_question`]; the
//! web UI keeps its own JS templates per existing convention but receives the
//! same serialized struct.

use crate::store::DecisionRecord;
use serde::Serialize;

use super::DecisionType;

/// A question rendered for display. Serializable so web/MCP responses can
/// carry it as-is.
#[derive(Debug, Clone, Serialize)]
pub struct RenderedQuestion {
    pub id: i64,
    pub decision_type: String,
    pub title: String,
    pub detail: String,
    /// `(value, label)` pairs: `value` is what to pass back as the answer.
    pub options: Vec<(String, String)>,
    pub priority: i64,
    pub created_at: i64,
}

/// Render one ledger row. Total: malformed params or an unknown type degrade to
/// a generic rendering, never an error — a question must always be displayable.
pub fn render_question(d: &DecisionRecord) -> RenderedQuestion {
    let params: serde_json::Value =
        serde_json::from_str(&d.params).unwrap_or(serde_json::Value::Null);
    let option_values: Vec<String> = serde_json::from_str(&d.options).unwrap_or_default();

    let (title, detail) = match DecisionType::parse(&d.decision_type) {
        Some(DecisionType::Classification) => classification_text(d, &params),
        Some(DecisionType::Duplicate) => duplicate_text(&params, &option_values),
        None => (
            format!("{}: {}", d.decision_type, d.subject),
            "This question was recorded by a newer indexa — update to see it properly.".to_owned(),
        ),
    };

    RenderedQuestion {
        id: d.id,
        decision_type: d.decision_type.clone(),
        title,
        detail,
        options: option_values
            .into_iter()
            .map(|v| {
                let label = option_label(&v);
                (v, label)
            })
            .collect(),
        priority: d.priority,
        created_at: d.created_at,
    }
}

fn classification_text(d: &DecisionRecord, params: &serde_json::Value) -> (String, String) {
    let suggested = d
        .auto_value
        .as_deref()
        .or_else(|| params.get("category").and_then(|v| v.as_str()))
        .unwrap_or("?");
    let confidence = d
        .confidence
        .map(f64::from)
        .or_else(|| params.get("confidence").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);
    let title = format!(
        "{} looks like {suggested} ({:.0}%)",
        d.subject,
        confidence * 100.0
    );
    // A re-ask row quotes the prior answer so the user decides with full context.
    let detail = match params.get("prior") {
        Some(prior) => {
            let prior_chosen = prior.get("chosen").and_then(|v| v.as_str()).unwrap_or("?");
            let when = prior
                .get("decided_at")
                .and_then(|v| v.as_i64())
                .map(ymd)
                .unwrap_or_else(|| "an earlier date".to_owned());
            format!(
                "You said {prior_chosen} on {when}; it now looks like {suggested} — \
                 keep {prior_chosen} or switch?"
            )
        }
        None => format!(
            "Confirm to keep {suggested}, pick another category to correct it, \
             or choose ignore to stop suggestions for this folder."
        ),
    };
    (title, detail)
}

fn duplicate_text(params: &serde_json::Value, option_values: &[String]) -> (String, String) {
    let member_count = params
        .get("paths")
        .and_then(|p| p.as_array())
        .map(|a| a.len())
        // Fall back to the options minus keep_all.
        .unwrap_or_else(|| option_values.len().saturating_sub(1));
    let title = format!("{member_count} files appear to be copies — which is canonical?");
    let evidence = if params
        .get("exact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        "Their content fingerprints are identical.".to_owned()
    } else {
        format!(
            "Their summaries are ~{:.0}% similar.",
            params
                .get("similarity")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                * 100.0
        )
    };
    let detail = format!(
        "{evidence} Picking a canonical copy down-weights the others to 0 in search \
         (reversible); keep_all leaves every copy searchable."
    );
    (title, detail)
}

/// Human label for an option value. Path/category values label themselves.
fn option_label(value: &str) -> String {
    match value {
        "ignore" => "Ignore (stop suggesting)".to_owned(),
        "keep_all" => "Keep all (no canonical)".to_owned(),
        other => other.to_owned(),
    }
}

/// Unix seconds → `YYYY-MM-DD` (UTC), via Howard Hinnant's civil-from-days —
/// not worth a chrono dependency for one display string.
fn ymd(ts: i64) -> String {
    if ts <= 0 {
        return "an earlier date".to_owned();
    }
    let days = ts.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(
        decision_type: &str,
        params: serde_json::Value,
        options: serde_json::Value,
    ) -> DecisionRecord {
        DecisionRecord {
            id: 5,
            decision_type: decision_type.to_owned(),
            subject: "/r/proj".to_owned(),
            params: params.to_string(),
            options: options.to_string(),
            auto_value: Some("code".to_owned()),
            chosen: None,
            source: None,
            confidence: Some(0.7),
            evidence_hash: "fp".to_owned(),
            priority: 50,
            status: "open".to_owned(),
            parent_id: None,
            superseded_by: None,
            effects: None,
            effects_applied_at: None,
            created_at: 1,
            decided_at: None,
        }
    }

    #[test]
    fn classification_question_renders_title_and_options() {
        let d = record(
            "classification",
            json!({"category": "code", "confidence": 0.7}),
            json!(["work", "code", "ignore"]),
        );
        let q = render_question(&d);
        assert_eq!(q.title, "/r/proj looks like code (70%)");
        assert_eq!(q.options.len(), 3);
        assert_eq!(
            q.options[2],
            ("ignore".to_owned(), "Ignore (stop suggesting)".to_owned())
        );
        assert!(q.detail.contains("Confirm"));
    }

    #[test]
    fn reask_question_quotes_the_prior_answer_and_date() {
        // 2026-01-15 00:00:00 UTC.
        let d = record(
            "classification",
            json!({
                "category": "code",
                "confidence": 0.9,
                "prior": {"chosen": "work", "decided_at": 1_768_435_200i64}
            }),
            json!(["work", "code", "ignore"]),
        );
        let q = render_question(&record("classification", json!({}), json!([])));
        assert!(!q.detail.contains("You said")); // sanity: plain row has no quote
        let q = render_question(&d);
        assert!(q.detail.contains("You said work on 2026-01-15"));
        assert!(q.detail.contains("it now looks like code"));
        assert!(q.detail.contains("keep work or switch?"));
    }

    #[test]
    fn duplicate_question_counts_members_and_labels_keep_all() {
        let d = record(
            "duplicate",
            json!({"paths": ["/r/a", "/r/b", "/r/c"], "exact": false, "similarity": 0.97}),
            json!(["/r/a", "/r/b", "/r/c", "keep_all"]),
        );
        let q = render_question(&d);
        assert_eq!(q.title, "3 files appear to be copies — which is canonical?");
        assert!(q.detail.contains("~97% similar"));
        assert_eq!(
            q.options[3],
            ("keep_all".to_owned(), "Keep all (no canonical)".to_owned())
        );
    }
}
