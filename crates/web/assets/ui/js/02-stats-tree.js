/* ── Stats ── */
async function loadStats() {
  try {
    const r = await fetch('/api/stats');
    const d = await r.json();
    let text = d.entries.toLocaleString() + ' files \xb7 ' + d.chunks.toLocaleString() + ' chunks';
    // Estimated token savings this week (≈4 bytes/token, same estimate as
    // `indexa status`); hidden until retrieval calls produce real savings.
    const u = d.usage_week;
    const statsEl = document.getElementById('stats');
    if (u && u.counterfactual > u.served) {
      const tokens = Math.round((u.counterfactual - u.served) / 4);
      text += ' \xb7 ~' + tokens.toLocaleString() + ' tokens saved this week';
      // The compact header drops the basis — restore it on hover (parity with
      // the CLI/MCP savings line).
      statsEl.title = 'Estimated: retrieval served ' + Math.round(u.served / 1024) +
        ' KB where whole-file context would have been ' + Math.round(u.counterfactual / 1024) +
        ' KB (≈4 bytes/token). See docs/methodology.md.';
    } else {
      statsEl.title = '';
    }
    statsEl.textContent = text;
  } catch(e) { document.getElementById('stats').textContent = 'No context yet'; }
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

