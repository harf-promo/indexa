//! Context Packs REST API.
//!
//! Routes:
//!   GET    /api/packs                  — list all packs
//!   POST   /api/packs                  — create a pack  { name, description? }
//!   DELETE /api/packs/:name            — delete a pack by name
//!   GET    /api/packs/:name/paths      — list paths in a pack
//!   POST   /api/packs/:name/paths      — add paths     { paths: [...] }
//!   DELETE /api/packs/:name/paths      — remove paths  { paths: [...] }
//!   GET    /api/packs/:name/items      — list paths with their inclusion mode (v0.78)
//!   POST   /api/packs/:name/items/mode — set one item's inclusion mode { path, mode }
//!   GET    /api/packs/:name/export     — export as XML/MD/JSON  ?format=&depth=&dry_run=
//!   POST   /api/packs/:name/refresh    — reindex stale members as a background job
//!   POST   /api/packs/:name/note       — attach a Markdown note { title, body }, best-effort indexed
//!   GET    /api/packs/:name/search     — search chunk content within the pack  ?q=&limit=
//!   POST   /api/packs/suggest          — suggest paths for a query { query, limit? }

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::dto::{err_json, JobStartResponse};
use crate::handlers::jobs::{job_slot_available, too_many_jobs, try_register_job};
use crate::jobs_exec::{
    finalize_done, run_deep_phase, scan_walk_config, schedule_cleanup, walk_for_job,
};
use crate::AppState;

// ── DTOs ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct PackDto {
    name: String,
    description: Option<String>,
    path_count: usize,
}

#[derive(Deserialize)]
pub(crate) struct CreatePackBody {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PathsBody {
    paths: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct ExportQuery {
    format: Option<String>,
    depth: Option<usize>,
    /// Relational slice: only files modified within this window (e.g. `7d`, `12h`, `90m`).
    changed_since: Option<String>,
    /// Relational slice: only files in this classification category (e.g. `code`, `document`).
    category: Option<String>,
    /// Append a call-graph section (which files relate to which), capped at 200 heaviest edges.
    #[serde(default)]
    include_graph: bool,
    /// With `include_graph=true`: `text` (per-format list, default) or `mermaid` (a fenced
    /// Mermaid flowchart block).
    graph_format: Option<String>,
    /// Preview only (v0.78): report the estimated token/byte cost as JSON instead of the
    /// rendered body, and record no "exported" pack event. Not supported with `format=okf`
    /// (this route never wrote an OKF bundle anyway — that's the CLI's job).
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
struct PackItemDto {
    path: String,
    inclusion_mode: String,
    /// Whether a pinned snapshot was actually captured (never the snapshot text itself — it can
    /// be large, and this endpoint backs a lightweight list view, not a content viewer).
    has_pinned_snapshot: bool,
}

#[derive(Deserialize)]
pub(crate) struct SetItemModeBody {
    path: String,
    mode: String,
}

#[derive(Deserialize)]
pub(crate) struct NoteBody {
    title: String,
    body: String,
}

#[derive(Deserialize)]
pub(crate) struct SuggestBody {
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct PackSearchQuery {
    q: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct SearchHitDto {
    path: String,
    heading: String,
    snippet: String,
    score: f64,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub(crate) async fn api_packs_list(State(state): State<AppState>) -> Response {
    let store = state.store.lock().await;
    match store.list_packs() {
        Ok(packs) => Json(
            packs
                .into_iter()
                .map(|p| PackDto {
                    name: p.name,
                    description: p.description,
                    path_count: p.path_count,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_create(
    State(state): State<AppState>,
    Json(body): Json<CreatePackBody>,
) -> Response {
    let mut store = state.store.lock().await;
    match store.create_pack(&body.name, body.description.as_deref()) {
        Ok(id) => Json(serde_json::json!({ "created": true, "id": id, "name": body.name }))
            .into_response(),
        Err(e) => {
            let msg = format!("{e:#}");
            // Surface unique-constraint violations as 409 so the UI can give a
            // clear "name already exists" message without parsing error text.
            if msg.contains("UNIQUE") {
                err_json(
                    StatusCode::CONFLICT,
                    format!("a pack named \"{}\" already exists", body.name),
                )
            } else {
                err_json(StatusCode::INTERNAL_SERVER_ERROR, msg)
            }
        }
    }
}

pub(crate) async fn api_packs_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let mut store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.delete_pack(&pack.id) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_paths_get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.pack_paths(&pack.id) {
        Ok(paths) => Json(serde_json::json!({ "name": name, "paths": paths })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_paths_add(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PathsBody>,
) -> Response {
    let mut store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.add_pack_paths(&pack.id, &body.paths) {
        Ok(()) => Json(serde_json::json!({ "added": body.paths.len() })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_paths_remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PathsBody>,
) -> Response {
    let mut store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.remove_pack_paths(&pack.id, &body.paths) {
        Ok(()) => Json(serde_json::json!({ "removed": body.paths.len() })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `GET /api/packs/:name/items` — list a pack's members with their per-item inclusion mode
/// (v0.78): `"reference"` (a live pointer, resolved fresh at export — the default, matching the
/// export behavior every pack item had before this field existed) or `"pinned"` (a frozen L2
/// snapshot). The richer sibling of `api_packs_paths_get`'s bare path list.
pub(crate) async fn api_packs_items_get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.pack_item_records(&pack.id) {
        Ok(items) => Json(serde_json::json!({
            "name": name,
            "items": items.into_iter().map(|i| PackItemDto {
                has_pinned_snapshot: i.pinned_snapshot.is_some(),
                path: i.path,
                inclusion_mode: i.inclusion_mode,
            }).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `POST /api/packs/:name/items/mode` — set one pack member's inclusion mode
/// (`{ "path": "...", "mode": "reference" | "pinned" }`). Switching to `"pinned"` captures the
/// item's CURRENT indexed (L2) content as the frozen snapshot; switching to `"reference"` clears
/// any previously-captured snapshot.
pub(crate) async fn api_packs_items_set_mode(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SetItemModeBody>,
) -> Response {
    let mut store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match store.set_pack_item_inclusion_mode(&pack.id, &body.path, &body.mode) {
        Ok(0) => err_json(
            StatusCode::NOT_FOUND,
            format!("no path \"{}\" in pack \"{name}\"", body.path),
        ),
        Ok(_) => Json(serde_json::json!({ "path": body.path, "inclusion_mode": body.mode }))
            .into_response(),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("invalid inclusion mode") {
                err_json(StatusCode::BAD_REQUEST, msg)
            } else {
                err_json(StatusCode::INTERNAL_SERVER_ERROR, msg)
            }
        }
    }
}

/// `GET /api/packs/:name/export` — render a pack as XML/Markdown/JSON.
///
/// Rendering is a long synchronous span: a freshness stat sweep over every member, then a
/// per-item `build_tree` + render loop. This used to run with `state.store` locked for the
/// whole time, which stalled EVERY other API request (health checks, SSE polling, the
/// Activity drawer) until the export finished. It now runs on a fresh, short-lived
/// connection inside `spawn_blocking`, matching `handlers/graph.rs` and
/// `handlers/insights_handler.rs`. The rendering itself is unchanged — it moved wholesale
/// into [`export_pack_response`].
pub(crate) async fn api_packs_export(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Response {
    let db_path = state.db_path.clone();
    tokio::task::spawn_blocking(move || match indexa_core::store::Store::open(&db_path) {
        Ok(mut store) => export_pack_response(&mut store, &name, &q),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    })
    .await
    .unwrap_or_else(|e| {
        err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("pack-export task panicked: {e}"),
        )
    })
}

/// The whole of the pack-export rendering, extracted verbatim from [`api_packs_export`] so it
/// can run inside `spawn_blocking` against its own connection. Synchronous by construction —
/// there is no `.await` anywhere in here, which is what makes the move safe.
fn export_pack_response(
    store: &mut indexa_core::store::Store,
    name: &str,
    q: &ExportQuery,
) -> Response {
    use indexa_query::{
        build_export_filter, build_tree, dry_run_report, prune_tree,
        relabel_pack_redaction_markers, render_graph, render_graph_mermaid, render_json,
        render_markdown, render_pinned_item, render_xml,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now = now_secs.to_string();

    let format = q.format.as_deref().unwrap_or("xml");
    let depth = q.depth;

    let pack = match store.pack_by_name(name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    // 404 (resource doesn't exist) takes priority over a 400 format/param validation error —
    // checked AFTER the pack lookup above so probing a typo'd pack name with
    // `dry_run=true&format=okf` reports "no such pack", not a param-combination complaint that
    // implies the pack itself was found.
    if q.dry_run && format == "okf" {
        return err_json(
            StatusCode::BAD_REQUEST,
            "dry_run is not supported with format=okf (this route doesn't write an OKF bundle \
             anyway — use the CLI `indexa pack export --format okf --out <dir>`)",
        );
    }
    // Per-item inclusion mode (v0.78): `items` carries each member's mode/pinned snapshot;
    // `paths` is kept alongside for the `--include-graph` scope below, which is mode-agnostic.
    let items = match store.pack_item_records(&pack.id) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let paths: Vec<String> = items.iter().map(|i| i.path.clone()).collect();
    if paths.is_empty() {
        return err_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("pack \"{name}\" is empty — add paths first"),
        );
    }

    // Relational slice (v0.60): same filters as CLI `pack export`, shared via build_export_filter.
    let allow = match build_export_filter(
        store,
        q.changed_since.as_deref(),
        q.category.as_deref(),
        now_secs,
    ) {
        Ok(a) => a,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };

    let is_xml = format != "md" && format != "markdown" && format != "json";
    let mut buf = String::new();
    if is_xml {
        // Freshness (G2b): same best-effort stat check the CLI/MCP export surfaces use (a stat
        // error must not fail an export that worked before) — confined to the XML path so
        // md/json exports (no wrapping header to hang the attribute off) don't pay the sweep.
        // This whole function runs inside `spawn_blocking` on its own connection, so the sweep
        // no longer blocks other requests.
        let stale_count = store.stale_pack_paths(&pack.id).unwrap_or_default().len();
        buf.push_str("<context pack=\"");
        buf.push_str(&indexa_core::text::xml_escape_attr(name));
        buf.push_str("\" generated=\"");
        buf.push_str(&now);
        buf.push_str("\" stale_files=\"");
        buf.push_str(&stale_count.to_string());
        buf.push_str("\">\n");
    }

    let mut exported = 0usize;
    for item in &items {
        let root_path = &item.path;
        // Per-item inclusion mode (v0.78): "pinned" renders the frozen L2 snapshot verbatim,
        // bypassing the live summaries tree and the relational slice (a pinned item deliberately
        // no longer tracks the live file metadata --changed-since/category filter on).
        // "reference" (and any legacy row, which defaults to it) is the unchanged pre-existing
        // behavior.
        if item.inclusion_mode == "pinned" {
            buf.push_str(&render_pinned_item(
                root_path,
                item.pinned_snapshot.as_deref(),
                format,
            ));
            buf.push('\n');
            exported += 1;
            continue;
        }
        let tree = match build_tree(store, root_path, depth) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        };
        // Apply the relational slice; skip a path that matched nothing.
        let tree = match &allow {
            Some(a) => match prune_tree(tree, a) {
                Some(t) => t,
                None => continue,
            },
            None => tree,
        };
        let rendered = match format {
            "md" | "markdown" => render_markdown(&tree),
            "json" => render_json(&tree),
            _ => render_xml(&tree, &now),
        };
        buf.push_str(&rendered);
        buf.push('\n');
        exported += 1;
    }

    // Optional call-graph section (1.6): a single pack path scopes the graph to it; multiple
    // paths fall back to the whole-index scope ("/"), matching CLI `indexa export
    // --include-graph`. Capped at 200 edges (the MCP `code_graph` tool's default).
    if q.include_graph {
        let scope = if paths.len() == 1 {
            paths[0].clone()
        } else {
            "/".to_owned()
        };
        if let Ok(graph) = store.code_graph(&scope, 200, false) {
            let section = if q.graph_format.as_deref() == Some("mermaid") {
                render_graph_mermaid(&graph)
            } else {
                render_graph(&graph, format)
            };
            buf.push_str(&section);
        }
    }

    if is_xml {
        buf.push_str("</context>\n");
    }

    if exported == 0 {
        let msg = if allow.is_some() {
            format!(
                "nothing in pack \"{name}\" matched the slice (changed_since / category) \
                 — widen it or drop the filter"
            )
        } else {
            format!(
                "no paths in pack \"{name}\" have summaries yet \
                 — run `indexa summarize <path>` first"
            )
        };
        return err_json(StatusCode::UNPROCESSABLE_ENTITY, msg);
    }

    // Scan exported content for secrets before it leaves the machine over HTTP — same invariant
    // the whole-tree export (api_export), MCP export_pack, and CLI `pack export` enforce. The
    // pack-only typed label relabel (v0.78, `[REDACTED-<kind>]` → `[redacted:<kind>]`) runs
    // strictly AFTER redact_secrets, so its output/count (AGENTS.md's pinned "redact count"
    // invariant) is unchanged — this only reformats marker text redact_secrets already produced.
    let (buf, _redacted) = indexa_query::redact::redact_secrets(&buf);
    let buf = relabel_pack_redaction_markers(&buf);

    // dry_run (v0.78): report the estimated size as JSON instead of the rendered body, and
    // record no "exported" event — nothing was actually exported.
    if q.dry_run {
        let report = dry_run_report(&buf);
        return Json(serde_json::json!({
            "name": name,
            "format": format,
            "approx_tokens": report.approx_tokens,
            "bytes": report.bytes,
            "items_exported": exported,
        }))
        .into_response();
    }

    let content_type = if format == "json" {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    // Best-effort (4.1) — a history-write hiccup must never fail an otherwise-successful export.
    let _ = store.record_pack_exported(&pack.id, format);
    ([(axum::http::header::CONTENT_TYPE, content_type)], buf).into_response()
}

/// `POST /api/packs/:name/refresh` — reindex a pack's stale members (files changed on disk
/// since last indexed) as a background job. No-op (no job started) when nothing is stale.
///
/// Design decision (flag-for-review, not auto-remove): mirrors the CLI's `cmd_pack_refresh_at`
/// — a stale member whose file has vanished from disk entirely is left in `pack_paths` rather
/// than silently dropped (same expire-don't-delete precedent `decisions::expire_vanished_decisions`
/// sets for the review ledger). It stays flagged (`stale_pack_paths` keeps reporting it) until
/// the user explicitly removes it. Safe to hand every stale path to the walk loop below
/// unconditionally because `walk()` (crates/core/src/walker.rs) is fail-open on an
/// unreadable/missing root — a vanished path just yields zero entries, not a job failure.
pub(crate) async fn api_packs_refresh(
    Path(name): Path<String>,
    State(s): State<AppState>,
) -> Response {
    // Fast-path 429 before any store work; the authoritative cap check is `try_register_job`.
    if let Some(resp) = job_slot_available(&s.jobs).await {
        return resp;
    }

    let stale = {
        let store = s.store.lock().await;
        let pack = match store.pack_by_name(&name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\""))
            }
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        };
        store.stale_pack_paths(&pack.id).unwrap_or_default()
        // Lock dropped here, before spawning — never held across an `.await`/`tokio::spawn`.
    };
    if stale.is_empty() {
        return Json(serde_json::json!({ "stale_files": 0 })).into_response();
    }
    let missing_count = stale
        .iter()
        .filter(|p| !std::path::Path::new(p).exists())
        .count();

    // Check the cap and register under one lock: a refresh racing another start for the last
    // slot passed the early check above too, and must not overshoot the cap.
    let Some((id, handle)) = try_register_job(&s.jobs, "pack_refresh", name.clone()).await else {
        return too_many_jobs();
    };
    let state = s.clone();
    tokio::spawn(async move {
        // Walk each stale path independently (ignore::WalkBuilder handles a bare file root
        // correctly — same behavior `cmd_deep` relies on for the CLI's `pack refresh`) and
        // concatenate the resulting per-file Vecs, reusing the existing, trusted per-file
        // classification/entry-construction logic rather than a new builder.
        let mut entries = Vec::with_capacity(stale.len());
        for path in &stale {
            if let Some(mut e) = walk_for_job(
                path,
                &handle,
                &state.walk_semaphore,
                scan_walk_config(&state.config.scan),
            )
            .await
            {
                entries.append(&mut e);
            }
        }
        if run_deep_phase(&state, &name, &entries, &handle).await {
            let reindexed = stale.len() - missing_count;
            let detail = format!("{reindexed} reindexed, {missing_count} vanished (left flagged)");
            {
                let mut store = state.store.lock().await;
                if let Ok(Some(pack)) = store.pack_by_name(&name) {
                    let _ = store.record_pack_refreshed(&pack.id, &detail);
                }
            }
            finalize_done(
                &handle,
                &format!(
                    "Pack refresh complete: {} file(s), {missing_count} vanished",
                    entries.len()
                ),
            );
        }
        schedule_cleanup(state.jobs.clone(), handle.id);
    });
    Json(JobStartResponse { job_id: id }).into_response()
}

/// `POST /api/packs/:name/note` — attach a Markdown note to a pack: write it under
/// `<data_dir>/notes/`, register it as a pack member, and best-effort reindex it as a background
/// job. The durable part (file written + pack membership recorded) happens under a scoped store
/// lock regardless of job capacity; the reindex is skipped (not failed) when the job cap is full —
/// the note is still saved and picked up by the next `pack refresh`/deep pass.
pub(crate) async fn api_packs_note(
    Path(name): Path<String>,
    State(s): State<AppState>,
    Json(body): Json<NoteBody>,
) -> Response {
    let data_dir = match s.db_path.parent() {
        Some(d) => d.to_path_buf(),
        None => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "index db path has no parent directory",
            )
        }
    };

    let note_path = {
        let mut store = s.store.lock().await;
        let pack = match store.pack_by_name(&name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\""))
            }
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        };
        // `write_note_file` is a sync filesystem write with no `.await` inside — kept in this
        // same lock scope (rather than re-acquiring after dropping) so there's no TOCTOU window
        // where the pack could be deleted between the existence check above and
        // `add_pack_paths` below. It's a single small file write, not the kind of slow op #471
        // fixed `api_packs_search` to release the lock around (that was a network-bound embed
        // call) — no `.await` crosses this scope, so it can't deadlock or stall other handlers
        // for long.
        let path =
            match indexa_core::notes::write_note_file(&data_dir, &name, &body.title, &body.body) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
            };
        if let Err(e) = store.add_pack_paths(&pack.id, std::slice::from_ref(&path)) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
        path
        // Lock dropped here, before any spawn — never held across an `.await`/`tokio::spawn`.
    };

    // Best-effort background reindex — skipped, not failed, when the job cap is full. The note is
    // already durably saved and registered above, so a busy job queue must not undo that. Unlike
    // `api_packs_refresh`, which answers 429 when the cap is full (a foreground reindex request),
    // here the reindex is backgrounded and optional: `try_register_job` checks the cap and
    // registers under one lock, and `None` just means "skip the reindex, the save happened".
    if let Some((id, handle)) = try_register_job(&s.jobs, "pack_note", name.clone()).await {
        let state = s.clone();
        let path_for_job = note_path.clone();
        tokio::spawn(async move {
            // Same single-path walk-then-deep shape as `api_packs_refresh`'s loop body (here over
            // just the one new note file): an empty `entries` still makes `run_deep_phase` report
            // success, and `walk_for_job` already finalizes the job itself on a real walk failure
            // (never call `finalize_done` after that — it would overwrite Failed with Done).
            let mut entries = Vec::new();
            if let Some(mut e) = walk_for_job(
                &path_for_job,
                &handle,
                &state.walk_semaphore,
                scan_walk_config(&state.config.scan),
            )
            .await
            {
                entries.append(&mut e);
            }
            if run_deep_phase(&state, &name, &entries, &handle).await {
                finalize_done(&handle, "Note indexed");
            }
            schedule_cleanup(state.jobs.clone(), handle.id);
        });
        return Json(serde_json::json!({ "saved": true, "path": note_path, "job_id": id }))
            .into_response();
    }

    Json(serde_json::json!({ "saved": true, "path": note_path })).into_response()
}

pub(crate) async fn api_packs_suggest(
    State(state): State<AppState>,
    Json(body): Json<SuggestBody>,
) -> Response {
    let limit = body.limit.unwrap_or(20).min(100);
    let query = body.query.trim().to_owned();
    if query.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "query must not be empty");
    }

    // Try semantic search via the embedder held in AppState.
    let embedding = state.embedder.embed(&query).await.ok();

    let store = state.store.lock().await;
    if let Some(emb) = embedding {
        match store.summary_cosine_search(&emb, limit, 0.15) {
            Ok(hits) if !hits.is_empty() => {
                let paths: Vec<&str> = hits.iter().map(|(p, _)| p.as_str()).collect();
                return Json(serde_json::json!({
                    "method": "semantic",
                    "paths": paths,
                }))
                .into_response();
            }
            Ok(_) => {} // fall through to keyword
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }

    // Keyword fallback.
    use indexa_core::config::HybridMode;
    match store.hybrid_search(&query, None, &HybridMode::Sparse, None, limit * 3, 0.0) {
        Ok(hits) => {
            let mut seen = std::collections::HashSet::new();
            let paths: Vec<&str> = hits
                .iter()
                .filter_map(|h| {
                    if seen.insert(h.entry_path.as_str()) {
                        Some(h.entry_path.as_str())
                    } else {
                        None
                    }
                })
                .take(limit)
                .collect();
            Json(serde_json::json!({
                "method": "keyword",
                "paths": paths,
            }))
            .into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub(crate) async fn api_packs_search(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<PackSearchQuery>,
) -> Response {
    use indexa_core::config::HybridMode;

    let query = match q.q.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => s.to_owned(),
        None => return err_json(StatusCode::BAD_REQUEST, "q parameter is required"),
    };
    let limit = q.limit.unwrap_or(20).min(100);

    // Embed the query BEFORE taking the store lock — the embed round-trip doesn't need the
    // store, and holding the mutex across it would block every other request (list/create/other
    // packs' search, etc.) for the embed call's full duration. (`api_packs_suggest` and
    // `api_packs_refresh` already follow this same release-before-slow-op shape.)
    let embedding = state.embedder.embed(&query).await.ok();

    let store = state.store.lock().await;
    let pack = match store.pack_by_name(&name) {
        Ok(Some(p)) => p,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, format!("no pack named \"{name}\"")),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let paths = match store.pack_paths(&pack.id) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    if paths.is_empty() {
        return Json(serde_json::json!({ "hits": [] })).into_response();
    }

    let per_scope = (limit * 2).max(10);
    let mut all_hits: Vec<indexa_core::store::SearchHit> = Vec::new();
    for root in &paths {
        if let Ok(mut hits) = store.hybrid_search(
            &query,
            embedding.as_deref(),
            &HybridMode::Rrf,
            Some(root.as_str()),
            per_scope,
            60.0,
        ) {
            all_hits.append(&mut hits);
        }
    }

    all_hits.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen = std::collections::HashSet::new();
    let hits: Vec<SearchHitDto> = all_hits
        .into_iter()
        .filter(|h| seen.insert(format!("{}:{}", h.entry_path, h.seq)))
        .take(limit)
        .map(|h| {
            let snippet: String = h.text.chars().take(200).collect();
            SearchHitDto {
                path: h.entry_path,
                heading: h.heading,
                snippet,
                score: h.rrf_score,
            }
        })
        .collect();

    Json(serde_json::json!({ "name": name, "query": query, "hits": hits })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::jobs::try_register_job;
    use crate::jobs::{JobHandle, JobStatus, Jobs};
    use indexa_core::store::{ChunkRecord, Store};
    use std::sync::Arc;
    use std::time::Duration;

    struct StubEmbedder;
    #[async_trait::async_trait]
    impl indexa_embed::Embedder for StubEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 8])
        }
        fn dim(&self) -> usize {
            8
        }
    }
    struct StubGenerator;
    #[async_trait::async_trait]
    impl indexa_llm::Generator for StubGenerator {
        async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
            Ok("stub".to_owned())
        }
    }

    /// A minimal `AppState`: a single walk permit (so a test can hold it and keep spawned pack
    /// jobs parked at their walk, i.e. still `Running`) and a scratch config path.
    fn state(store: Store, dir: &std::path::Path) -> AppState {
        let (_tx, telemetry) = tokio::sync::watch::channel(crate::dto::TelemetrySample::default());
        AppState {
            store: Arc::new(tokio::sync::Mutex::new(store)),
            embedder: Arc::new(StubEmbedder),
            llm: Arc::new(StubGenerator),
            config: Arc::new(indexa_core::config::Config::default()),
            jobs: Default::default(),
            db_path: Arc::new(dir.join("index.db")),
            config_path: Arc::new(dir.join("config.toml")),
            log_dir: Arc::new(dir.to_path_buf()),
            walk_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            machine_spec: Arc::new(indexa_core::resource::detect_machine()),
            telemetry,
            ann: Default::default(),
            ann_build_lock: Default::default(),
            watch_sessions: Default::default(),
        }
    }

    /// Fill the job registry to its cap, then finish one job so exactly ONE slot is free.
    async fn leave_one_free_slot(jobs: &Jobs) -> Vec<Arc<JobHandle>> {
        let mut held = Vec::new();
        while let Some((_, handle)) = try_register_job(jobs, "scan", "/busy".into()).await {
            held.push(handle);
        }
        held[0].set_status(JobStatus::Done);
        held
    }

    fn running_jobs(jobs: &std::collections::HashMap<uuid::Uuid, Arc<JobHandle>>) -> usize {
        jobs.values()
            .filter(|h| h.status() == JobStatus::Running)
            .count()
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_refreshes_into_the_last_slot_register_exactly_one_job() {
        // A refresh used to check the job cap, then do its store work, then register — so two
        // refreshes racing for the last free slot both passed the check and both registered,
        // overshooting the cap.
        let dir = tempfile::tempdir().unwrap();
        // An indexed member that has vanished from disk counts as stale (and its walk is empty).
        let gone = dir.path().join("gone.rs").to_string_lossy().into_owned();
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_chunks(&[ChunkRecord {
                entry_path: gone.clone(),
                seq: 0,
                heading: String::new(),
                text: "fn gone() {}".to_owned(),
                language: None,
                embedding: Some(vec![0.0; 8]),
                embed_model: Some("test".to_owned()),
                content_hash: None,
            }])
            .unwrap();
        let pack_id = store.create_pack("race", None).unwrap();
        store.add_pack_paths(&pack_id, &[gone]).unwrap();
        let s = state(store, dir.path());
        let held = leave_one_free_slot(&s.jobs).await;

        // Park every spawned refresh job at its walk so it stays `Running` (holding its slot).
        let _walk = s.walk_semaphore.clone().acquire_owned().await.unwrap();
        // Hold the store lock so both requests reach the same point before either registers.
        let store_guard = s.store.lock().await;
        let requests: Vec<_> = (0..2)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move { api_packs_refresh(Path("race".into()), State(s)).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(store_guard);

        let mut started = 0;
        let mut rejected = 0;
        for request in requests {
            let resp = request.await.unwrap();
            match resp.status() {
                StatusCode::OK => {
                    let json = body_json(resp).await;
                    assert!(json.get("job_id").is_some(), "{json}");
                    started += 1;
                }
                StatusCode::TOO_MANY_REQUESTS => rejected += 1,
                other => panic!("unexpected status {other}"),
            }
        }
        assert_eq!((started, rejected), (1, 1));
        assert_eq!(running_jobs(&*s.jobs.read().await), held.len());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_notes_into_the_last_slot_save_both_and_start_one_job() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store.create_pack("notes", None).unwrap();
        let s = state(store, dir.path());
        let held = leave_one_free_slot(&s.jobs).await;

        let _walk = s.walk_semaphore.clone().acquire_owned().await.unwrap();
        let store_guard = s.store.lock().await;
        let requests: Vec<_> = (0..2)
            .map(|i| {
                let s = s.clone();
                let body = NoteBody {
                    title: format!("Note {i}"),
                    body: "body".to_owned(),
                };
                tokio::spawn(async move {
                    api_packs_note(Path("notes".into()), State(s), Json(body)).await
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(store_guard);

        let mut jobs_started = 0;
        for request in requests {
            let resp = request.await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let json = body_json(resp).await;
            assert_eq!(json["saved"], true, "{json}");
            if json.get("job_id").is_some() {
                jobs_started += 1;
            }
        }
        assert_eq!(jobs_started, 1);
        assert_eq!(running_jobs(&*s.jobs.read().await), held.len());
    }
}
