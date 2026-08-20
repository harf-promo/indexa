/* ── Stats ── */
async function loadStats() {
  try {
    // `/api/stats`'s entries/chunks/summaries are unfiltered `COUNT(*)`s over tables that
    // hold file AND dir rows side by side, so neither `entries` nor `summaries` is a true
    // file-only or folder-only figure. `/api/map` already computes real per-kind counts
    // (`total_files`, and directory coverage as `built`/`total_dirs`) — use those instead
    // of inventing a new backend query. `d.chunks` and `d.usage_week` still come from
    // `/api/stats` (chunks are file-only by construction; usage isn't in `/api/map`).
    const [statsR, mapR] = await Promise.all([fetch('/api/stats'), fetch('/api/map')]);
    // `r.ok` matters here, not just a resolved fetch: a server error still returns valid
    // JSON (`{"error": …}` from `err_json`), so `.json()` would otherwise resolve as if
    // it were real data and render a false "0 files … 0 of 0 folders (0%)". Route either
    // endpoint's failure into the existing catch below instead.
    if (!statsR.ok || !mapR.ok) throw new Error('stats/map unavailable');
    const d = await statsR.json();
    const m = await mapR.json();
    const totalFiles = m.total_files || 0;
    const totalDirs = m.total_dirs || 0;
    const builtDirs = m.built || 0;
    const pct = totalDirs > 0 ? Math.round((100 * builtDirs) / totalDirs) : 0;
    const text = totalFiles.toLocaleString() + ' files \xb7 ' +
      d.chunks.toLocaleString() + ' chunks \xb7 ' +
      builtDirs.toLocaleString() + ' of ' + totalDirs.toLocaleString() + ' folders summarized (' + pct + '%)';
    const statsEl = document.getElementById('stats');
    if (statsEl) {
      statsEl.textContent = text;
      statsEl.title = 'Files indexed and searchable chunks vs folders with an AI summary built — Ask works on chunks; Export and folder overviews need folder summaries.';
    }
    renderSavingsWidget(d.usage_week);
  } catch(e) { document.getElementById('stats').textContent = 'No context yet'; }
}

/* Promote the token-savings figure from a topbar suffix to a dedicated engine-bar
   widget: "~N tokens saved/wk", with the honest estimate basis on hover. Hidden
   until retrieval has actually served something (counterfactual > served). The
   number is an estimate (≈4 bytes/token, same as `indexa status`/methodology). */
function renderSavingsWidget(u) {
  const wrap = document.getElementById('engine-savings');
  const val = document.getElementById('engine-savings-val');
  if (!wrap || !val) return;
  if (u && u.counterfactual > u.served) {
    const tokens = Math.round((u.counterfactual - u.served) / 4);
    val.textContent = '~' + tokens.toLocaleString() + ' tok/wk';
    wrap.title = 'Estimated tokens saved this week: retrieval served ' +
      Math.round(u.served / 1024) + ' KB where whole-file context would have been ' +
      Math.round(u.counterfactual / 1024) + ' KB (≈4 bytes/token, estimated — see docs/methodology.md).';
    wrap.hidden = false;
  } else {
    wrap.hidden = true;
  }
}

/* ── Tree ── */
async function loadTreeLevel(parentPath, container) {
  container.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:12px">Loading…</div>';
  try {
    const url = '/api/tree?path=' + encodeURIComponent(parentPath);
    const r = await fetch(url);
    const nodes = await r.json();
    if (!nodes.length) {
      container.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:12px">Empty</div>';
      return;
    }
    container.innerHTML = '';
    nodes.forEach(function(node) { container.appendChild(buildTreeNode(node)); });
  } catch(e) {
    container.innerHTML = '<div style="padding:6px 12px;color:var(--red);font-size:12px">Error loading</div>';
  }
}

