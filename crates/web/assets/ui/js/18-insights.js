// ── Insights (v0.10) ──────────────────────────────────────────────────────────

function runInsights(kind) {  // eslint-disable-line no-unused-vars
  var el = document.getElementById('insights-results');
  if (!el) return;
  el.style.display = '';
  el.textContent = 'Running ' + kind + ' analysis…';

  var url = '/api/insights/' + kind;
  if (kind === 'duplicates') url += '?threshold=0.90';
  if (kind === 'stale')      url += '?days=365';
  if (kind === 'diff')       url += '?days=7';

  fetch(url)
    .then(function (r) { return r.json(); })
    .then(function (d) { renderInsightsResult(kind, d, el); })
    .catch(function (e) { el.textContent = 'Error: ' + e.message; });
}

function renderInsightsResult(kind, d, el) {
  if (kind === 'duplicates') {
    var clusters = d.clusters || [];
    if (clusters.length === 0) {
      el.innerHTML = '<span style="color:var(--muted)">No duplicates found.</span>';
      return;
    }
    el.innerHTML = '<strong>' + clusters.length + ' duplicate cluster(s):</strong>'
      + clusters.map(function (c, i) {
        return '<div style="margin-top:8px"><em>Cluster ' + (i + 1) + ' — similarity ' + (c.similarity * 100).toFixed(0) + '%</em>'
          + '<ul style="margin:2px 0 0 16px;padding:0">'
          + c.paths.map(function (p) { return '<li style="list-style:disc">' + escI(p) + '</li>'; }).join('')
          + '</ul></div>';
      }).join('');
  } else if (kind === 'stale') {
    var entries = d.entries || [];
    if (entries.length === 0) {
      el.innerHTML = '<span style="color:var(--muted)">No stale projects found.</span>';
      return;
    }
    el.innerHTML = '<strong>' + entries.length + ' stale director(ies) (not modified in 365+ days):</strong>'
      + '<table style="width:100%;border-collapse:collapse;margin-top:6px">'
      + entries.map(function (e) {
        return '<tr style="border-top:1px solid var(--border)">'
          + '<td style="padding:2px 6px;color:var(--muted)">' + e.days_since_modified + 'd</td>'
          + '<td style="padding:2px 6px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">' + escI(e.path) + '</td>'
          + '</tr>';
      }).join('')
      + '</table>';
  } else if (kind === 'diff') {
    var added = d.added || [];
    var modified = d.modified || [];
    el.innerHTML = '<strong>Last 7 days:</strong>'
      + '<div style="margin-top:6px"><em>Added (' + d.added_count + '):</em>'
      + (added.length === 0 ? ' <span style="color:var(--muted)">none</span>' : '<ul style="margin:2px 0 0 16px;padding:0">' + added.map(function (p) { return '<li style="list-style:circle;color:var(--green)">+ ' + escI(p) + '</li>'; }).join('') + '</ul>')
      + '</div><div style="margin-top:6px"><em>Modified (' + d.modified_count + '):</em>'
      + (modified.length === 0 ? ' <span style="color:var(--muted)">none</span>' : '<ul style="margin:2px 0 0 16px;padding:0">' + modified.map(function (p) { return '<li style="list-style:circle;color:var(--accent)">~ ' + escI(p) + '</li>'; }).join('') + '</ul>')
      + '</div>';
  }
}

function escI(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}
