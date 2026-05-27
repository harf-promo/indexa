//! Local web server — axum-based API + embedded HTML/JS UI.
//!
//! Serves at `http://localhost:<port>` with:
//! - `GET /`             — the single-page UI
//! - `GET /api/stats`    — { entries, chunks }
//! - `GET /api/map`      — [{ category, entry_count, total_size }]
//! - `POST /api/ask`     — { question } → { answer, sources }

use anyhow::Result;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use indexa_core::{config::Config, store::Store};
use indexa_embed::Embedder;
use indexa_llm::Generator;
use indexa_query::{synthesize_from_hits, QaConfig};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    store: Arc<Mutex<Store>>,
    embedder: Arc<dyn Embedder + Send + Sync + 'static>,
    llm: Arc<dyn Generator + Send + Sync + 'static>,
    config: Arc<Config>,
}

// ── API types ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatsResponse {
    entries: u64,
    chunks: u64,
}

#[derive(Serialize)]
struct MapRow {
    category: String,
    entry_count: u64,
    total_size: u64,
}

#[derive(Deserialize)]
struct AskRequest {
    question: String,
}

#[derive(Serialize)]
struct AskResponse {
    answer: String,
    sources: Vec<AskSource>,
}

#[derive(Serialize)]
struct AskSource {
    path: String,
    heading: String,
    snippet: String,
}

// ── Route handlers ────────────────────────────────────────────────────────────

async fn serve_ui() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_HTML,
    )
        .into_response()
}

async fn api_stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let store = state.store.lock().await;
    let entries = store.entry_count().unwrap_or(0);
    let chunks = store.chunk_count().unwrap_or(0);
    Json(StatsResponse { entries, chunks })
}

async fn api_map(State(state): State<AppState>) -> Json<Vec<MapRow>> {
    let store = state.store.lock().await;
    let rows = store
        .region_summary()
        .unwrap_or_default()
        .into_iter()
        .map(|r| MapRow {
            category: r.category,
            entry_count: r.entry_count,
            total_size: r.total_size,
        })
        .collect();
    Json(rows)
}

async fn api_ask(State(state): State<AppState>, Json(body): Json<AskRequest>) -> Response {
    let qa_cfg = QaConfig {
        top_k: state.config.retrieval.top_k,
        ..QaConfig::default()
    };

    // Step 1: embed query (async, no store lock needed).
    let query_vec = match state.embedder.embed(&body.question).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // Step 2: sync store query (hold lock only for the synchronous call, no await).
    let hits = {
        let store = state.store.lock().await;
        match store.hybrid_search(&body.question, Some(&query_vec), qa_cfg.top_k) {
            Ok(h) => h,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
    }; // MutexGuard dropped here — no store reference held across awaits

    // Step 3: LLM synthesis (async, store lock already released).
    match synthesize_from_hits(hits, state.llm.as_ref(), &body.question, &qa_cfg).await {
        Ok(answer) => Json(AskResponse {
            answer: answer.answer,
            sources: answer
                .sources
                .into_iter()
                .map(|s| AskSource {
                    path: s.path,
                    heading: s.heading,
                    snippet: s.snippet,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Start the web UI server on `port`. Runs until Ctrl-C or the process exits.
pub async fn serve(
    port: u16,
    store: Store,
    embedder: Arc<dyn Embedder + Send + Sync + 'static>,
    llm: Arc<dyn Generator + Send + Sync + 'static>,
    config: Config,
) -> Result<()> {
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        embedder,
        llm,
        config: Arc::new(config),
    };

    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/api/stats", get(api_stats))
        .route("/api/map", get(api_map))
        .route("/api/ask", post(api_ask))
        .with_state(state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers([header::CONTENT_TYPE]),
        );

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Indexa web UI listening on http://{addr}");
    println!("Open http://localhost:{port} in your browser. Press Ctrl-C to stop.");

    axum::serve(listener, app).await?;
    Ok(())
}

// ── Embedded UI ───────────────────────────────────────────────────────────────

const UI_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Indexa</title>
<style>
  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
  :root {
    --bg: #0d1117; --surface: #161b22; --border: #30363d;
    --text: #e6edf3; --muted: #8b949e; --accent: #58a6ff;
    --green: #3fb950; --red: #f85149; --orange: #d29922;
    font-size: 14px;
  }
  body { background: var(--bg); color: var(--text); font-family: 'SF Mono', 'Cascadia Code', 'Fira Code', monospace; min-height: 100vh; }
  header { background: var(--surface); border-bottom: 1px solid var(--border); padding: 12px 24px; display: flex; align-items: center; gap: 16px; }
  header h1 { font-size: 18px; font-weight: 600; color: var(--accent); letter-spacing: -0.5px; }
  header .stats { color: var(--muted); font-size: 12px; }
  .layout { display: grid; grid-template-columns: 280px 1fr; height: calc(100vh - 49px); }
  .sidebar { background: var(--surface); border-right: 1px solid var(--border); overflow-y: auto; padding: 16px 0; }
  .sidebar h2 { font-size: 11px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted); padding: 0 16px 8px; }
  .map-row { display: flex; justify-content: space-between; padding: 6px 16px; cursor: default; }
  .map-row:hover { background: rgba(88,166,255,0.05); }
  .map-row .cat { color: var(--text); }
  .map-row .count { color: var(--muted); font-size: 12px; }
  .main { display: flex; flex-direction: column; overflow: hidden; }
  .chat-area { flex: 1; overflow-y: auto; padding: 24px; display: flex; flex-direction: column; gap: 20px; }
  .welcome { max-width: 600px; margin: auto; text-align: center; padding: 60px 0; }
  .welcome h2 { font-size: 22px; color: var(--text); margin-bottom: 12px; font-weight: 500; }
  .welcome p { color: var(--muted); line-height: 1.6; }
  .welcome code { color: var(--accent); }
  .msg { max-width: 800px; width: 100%; }
  .msg.user .bubble { background: var(--accent); color: #0d1117; border-radius: 12px 12px 2px 12px; padding: 10px 14px; display: inline-block; }
  .msg.assistant .bubble { background: var(--surface); border: 1px solid var(--border); border-radius: 2px 12px 12px 12px; padding: 14px; white-space: pre-wrap; line-height: 1.7; }
  .msg.user { align-self: flex-end; }
  .msg.assistant { align-self: flex-start; }
  .sources { margin-top: 12px; }
  .sources h4 { font-size: 11px; text-transform: uppercase; letter-spacing: 1px; color: var(--muted); margin-bottom: 8px; }
  .source-item { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 8px 12px; margin-bottom: 6px; font-size: 12px; }
  .source-item .path { color: var(--accent); font-weight: 500; }
  .source-item .heading { color: var(--orange); margin-left: 8px; }
  .source-item .snippet { color: var(--muted); margin-top: 4px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .thinking { color: var(--muted); font-style: italic; animation: pulse 1.5s ease-in-out infinite; }
  @keyframes pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.4; } }
  .input-bar { background: var(--surface); border-top: 1px solid var(--border); padding: 16px 24px; display: flex; gap: 10px; }
  .input-bar input { flex: 1; background: var(--bg); border: 1px solid var(--border); border-radius: 8px; color: var(--text); padding: 10px 14px; font-family: inherit; font-size: 14px; outline: none; }
  .input-bar input:focus { border-color: var(--accent); }
  .input-bar input::placeholder { color: var(--muted); }
  .input-bar button { background: var(--accent); color: #0d1117; border: none; border-radius: 8px; padding: 10px 20px; font-family: inherit; font-size: 14px; font-weight: 600; cursor: pointer; }
  .input-bar button:hover { opacity: 0.85; }
  .input-bar button:disabled { opacity: 0.4; cursor: default; }
  ::-webkit-scrollbar { width: 6px; } ::-webkit-scrollbar-track { background: transparent; } ::-webkit-scrollbar-thumb { background: var(--border); border-radius: 3px; }
</style>
</head>
<body>
<header>
  <h1>&#x2B21; Indexa</h1>
  <span class="stats" id="stats">Loading&#x2026;</span>
</header>
<div class="layout">
  <aside class="sidebar">
    <h2>Disk map</h2>
    <div id="map-rows"></div>
  </aside>
  <main class="main">
    <div class="chat-area" id="chat">
      <div class="welcome">
        <h2>Ask your computer anything</h2>
        <p>Indexa has read and indexed your files. Ask a question in plain English &mdash; like<br>
        <code>&ldquo;where are my tax documents?&rdquo;</code> or <code>&ldquo;show me Python files that use async&rdquo;</code>.</p>
      </div>
    </div>
    <div class="input-bar">
      <input type="text" id="q" placeholder="Ask a question about your files&hellip;" autocomplete="off">
      <button id="send">Ask</button>
    </div>
  </main>
</div>
<script>
const chat = document.getElementById('chat');
const qInput = document.getElementById('q');
const sendBtn = document.getElementById('send');

async function loadStats() {
  try {
    const r = await fetch('/api/stats');
    const d = await r.json();
    document.getElementById('stats').textContent =
      d.entries.toLocaleString() + ' files · ' + d.chunks.toLocaleString() + ' indexed chunks';
  } catch(e) { document.getElementById('stats').textContent = 'No index yet'; }
}

async function loadMap() {
  try {
    const r = await fetch('/api/map');
    const rows = await r.json();
    const el = document.getElementById('map-rows');
    if (!rows.length) { el.innerHTML = '<div style="padding:8px 16px;color:var(--muted);font-size:12px">Run indexa scan first</div>'; return; }
    el.innerHTML = rows.map(function(row) {
      return '<div class="map-row"><span class="cat">' + escapeHtml(row.category) + '</span><span class="count">' + row.entry_count.toLocaleString() + '</span></div>';
    }).join('');
  } catch(e) {}
}

function appendMsg(role, html) {
  const welcome = chat.querySelector('.welcome');
  if (welcome) welcome.remove();
  const div = document.createElement('div');
  div.className = 'msg ' + role;
  div.innerHTML = '<div class="bubble">' + html + '</div>';
  chat.appendChild(div);
  chat.scrollTop = chat.scrollHeight;
  return div;
}

function escapeHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

async function doAsk() {
  const q = qInput.value.trim();
  if (!q) return;
  qInput.value = '';
  sendBtn.disabled = true;

  appendMsg('user', escapeHtml(q));
  const thinking = appendMsg('assistant', '<span class="thinking">Thinking…</span>');

  try {
    const r = await fetch('/api/ask', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ question: q })
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || 'Request failed');

    let html = escapeHtml(d.answer);
    if (d.sources && d.sources.length > 0) {
      html += '<div class="sources"><h4>Sources</h4>' +
        d.sources.map(function(s) {
          return '<div class="source-item"><span class="path">' + escapeHtml(s.path) + '</span>' +
            (s.heading ? '<span class="heading">' + escapeHtml(s.heading) + '</span>' : '') +
            '<div class="snippet">' + escapeHtml(s.snippet) + '</div></div>';
        }).join('') + '</div>';
    }
    thinking.querySelector('.bubble').innerHTML = html;
  } catch(err) {
    thinking.querySelector('.bubble').innerHTML = '<span style="color:var(--red)">' + escapeHtml(err.message) + '</span>';
  }

  sendBtn.disabled = false;
  qInput.focus();
  chat.scrollTop = chat.scrollHeight;
}

sendBtn.addEventListener('click', doAsk);
qInput.addEventListener('keydown', function(e) { if (e.key === 'Enter') doAsk(); });

loadStats();
loadMap();
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_html_is_non_empty() {
        assert!(!UI_HTML.is_empty());
        assert!(UI_HTML.contains("Indexa"));
        assert!(UI_HTML.contains("/api/ask"));
    }
}
