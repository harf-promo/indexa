/* ── Stats ── */
async function loadStats() {
  try {
    const r = await fetch('/api/stats');
    const d = await r.json();
    const text = d.entries.toLocaleString() + ' files \xb7 ' + d.chunks.toLocaleString() + ' chunks';
    const statsEl = document.getElementById('stats');
    if (statsEl) statsEl.textContent = text;
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

