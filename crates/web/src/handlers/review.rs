//! Decision Ledger review inbox API (v0.22).
//!
//! Routes:
//!   GET  /api/review          — open questions, rendered for display { type?, limit? }
//!   POST /api/review/answer   — answer an open question { id, chosen }
//!   POST /api/review/dismiss  — dismiss an open question { id }
//!   GET  /api/review/history  — full revision chain for a subject { subject, type? }
//!   POST /api/review/revert   — restore a superseded decided revision's answer { id }
//!   GET  /api/review/count    — open-question count (polled for the topbar badge)
//!
//! Answers route through `decide_and_apply` — the same single entry point the
//! CLI and MCP use — so the web surface inherits the crash-safety contract
//! (ledger row commits first, idempotent projection second, receipt last).

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use indexa_core::decisions::{
    batch_answer_refusal, decide_and_apply, effects, revert_decision, templates::render_question,
    DecisionType,
};
use indexa_core::store::DecisionRecord;
use serde::Deserialize;

use crate::dto::err_json;
use crate::AppState;

#[derive(Deserialize)]
pub(crate) struct ReviewListQuery {
    /// Filter to one decision type; omit for all.
    r#type: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct AnswerBody {
    id: i64,
    chosen: String,
}

#[derive(Deserialize)]
pub(crate) struct DismissBody {
    id: i64,
}

#[derive(Deserialize)]
pub(crate) struct RevertBody {
    id: i64,
}

#[derive(Deserialize)]
pub(crate) struct BatchAnswerBody {
    /// Decision type to batch-answer (`classification` / `archive` / …).
    #[serde(rename = "type")]
    decision_type: String,
    /// Path prefix — every OPEN question of `decision_type` whose subject is
    /// under this directory is answered.
    under: String,
    /// The answer to apply to all of them. Must be batch-safe for the type
    /// (see `batch_answer_refusal`).
    chosen: String,
}

#[derive(Deserialize)]
pub(crate) struct HistoryQuery {
    subject: String,
    /// Filter to one decision type; omit to walk every known type.
    r#type: Option<String>,
}

/// Reject a type filter the ledger doesn't know — a typo'd filter would
/// otherwise match nothing and read as "inbox zero".
#[allow(clippy::result_large_err)] // Response is the natural err type for axum handlers
fn validate_type(t: &str) -> Result<(), Response> {
    if DecisionType::parse(t).is_none() {
        let known = DecisionType::ALL.map(|t| t.as_str()).join(", ");
        return Err(err_json(
            StatusCode::BAD_REQUEST,
            format!("unknown decision type '{t}' (known: {known})"),
        ));
    }
    Ok(())
}

/// Raw ledger row → JSON. `params`/`options`/`effects` are stored as JSON text;
/// parse them so clients get structures, not double-encoded strings.
fn decision_json(d: &DecisionRecord) -> serde_json::Value {
    serde_json::json!({
        "id": d.id,
        "decision_type": d.decision_type,
        "subject": d.subject,
        "params": serde_json::from_str::<serde_json::Value>(&d.params)
            .unwrap_or(serde_json::Value::Null),
        "options": serde_json::from_str::<serde_json::Value>(&d.options)
            .unwrap_or(serde_json::Value::Null),
        "auto_value": d.auto_value,
        "chosen": d.chosen,
        "source": d.source,
        "confidence": d.confidence,
        "evidence_hash": d.evidence_hash,
        "priority": d.priority,
        "status": d.status,
        "parent_id": d.parent_id,
        "superseded_by": d.superseded_by,
        "effects": d.effects.as_deref()
            .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok()),
        "effects_applied_at": d.effects_applied_at,
        "created_at": d.created_at,
        "decided_at": d.decided_at,
    })
}

/// List open questions in inbox order, rendered for display.
pub(crate) async fn api_review_list(
    State(state): State<AppState>,
    Query(q): Query<ReviewListQuery>,
) -> Response {
    if let Some(t) = q.r#type.as_deref() {
        if let Err(resp) = validate_type(t) {
            return resp;
        }
    }
    let limit = q.limit.unwrap_or(50).min(200);
    let store = state.store.lock().await;
    match store.open_decisions(q.r#type.as_deref(), limit) {
        Ok(rows) => Json(rows.iter().map(render_question).collect::<Vec<_>>()).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Answer an open question and apply its projection.
pub(crate) async fn api_review_answer(
    State(state): State<AppState>,
    Json(body): Json<AnswerBody>,
) -> Response {
    let mut store = state.store.lock().await;
    match decide_and_apply(&mut store, body.id, &body.chosen, "user") {
        Ok(effects) => {
            Json(serde_json::json!({ "answered": true, "effects": effects })).into_response()
        }
        // Unknown id, off-menu choice, or an already-answered race (another
        // surface got there first) — all client-input shaped, never a 500.
        Err(e) => err_json(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// Answer every open question of a type under a directory at once. Mirrors the
/// CLI `review answer --type T --under DIR --choose V`: validate batch-safety,
/// answer the matching open rows, then project each (a failed projection is left
/// for the repair sweep, never blocking the rest).
pub(crate) async fn api_review_answer_batch(
    State(state): State<AppState>,
    Json(body): Json<BatchAnswerBody>,
) -> Response {
    let Some(ty) = DecisionType::parse(&body.decision_type) else {
        return err_json(
            StatusCode::BAD_REQUEST,
            format!("unknown decision type '{}'", body.decision_type),
        );
    };
    if let Some(msg) = batch_answer_refusal(ty, &body.chosen) {
        return err_json(StatusCode::BAD_REQUEST, msg);
    }
    let mut store = state.store.lock().await;
    let ids = match store.answer_decisions_under(&body.under, ty.as_str(), &body.chosen, "user") {
        Ok(ids) => ids,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let mut applied = 0usize;
    for aid in &ids {
        let Ok(Some(d)) = store.decision_by_id(*aid) else {
            continue;
        };
        if let Ok(e) = effects::apply_decision_effects(&mut store, &d) {
            let _ = store.mark_effects_applied(*aid, &e);
            applied += 1;
        }
    }
    Json(serde_json::json!({ "answered": ids.len(), "applied": applied })).into_response()
}

/// Dismiss an open question (sticky: returns only when the evidence changes).
pub(crate) async fn api_review_dismiss(
    State(state): State<AppState>,
    Json(body): Json<DismissBody>,
) -> Response {
    let mut store = state.store.lock().await;
    match store.dismiss_decision(body.id) {
        Ok(()) => Json(serde_json::json!({ "dismissed": true })).into_response(),
        Err(e) => err_json(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// Restore a superseded decided revision's answer (the "Restore this answer"
/// button in the history chain). Routes through the same
/// `core::decisions::revert_decision` the CLI uses, so the append-only chain
/// rules and projection contract can't drift between surfaces.
pub(crate) async fn api_review_revert(
    State(state): State<AppState>,
    Json(body): Json<RevertBody>,
) -> Response {
    let mut store = state.store.lock().await;
    match revert_decision(&mut store, body.id) {
        Ok(out) => Json(serde_json::json!({
            "reverted": true,
            "new_id": out.new_id,
            "superseded_id": out.superseded_id,
            "subject": out.subject,
            "chosen": out.chosen,
            "effects": out.effects,
        }))
        .into_response(),
        // Unknown id, a non-decided row, or a blocking open question — all
        // client-input shaped, never a 500.
        Err(e) => err_json(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// Every revision recorded for a subject, oldest first.
pub(crate) async fn api_review_history(
    State(state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if let Some(t) = q.r#type.as_deref() {
        if let Err(resp) = validate_type(t) {
            return resp;
        }
    }
    let store = state.store.lock().await;
    let history = (|| -> anyhow::Result<Vec<DecisionRecord>> {
        let mut rows = match q.r#type.as_deref() {
            Some(t) => store.decision_history(t, &q.subject)?,
            // Subjects are type-scoped in the ledger; with no filter, walk every
            // known type and merge by id so the chain reads in insertion order.
            None => {
                let mut all = Vec::new();
                for t in DecisionType::ALL {
                    all.extend(store.decision_history(t.as_str(), &q.subject)?);
                }
                all
            }
        };
        rows.sort_by_key(|d| d.id);
        Ok(rows)
    })();
    match history {
        Ok(rows) => Json(rows.iter().map(decision_json).collect::<Vec<_>>()).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Open-question count — cheap; the UI polls it for the topbar badge.
pub(crate) async fn api_review_count(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.open_decision_count() {
        Ok(n) => Json(serde_json::json!({ "open": n })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
