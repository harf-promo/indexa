// ── Insights (v0.10) ──────────────────────────────────────────────────────────

// Last-rendered results, kept so "Don't ask about this" buttons can reference
// their cluster/entry by index — onclick handlers carry only an integer, never
// a user-derived path (which would need escaping inside an attribute).
var _insightsLast = { clusters: [], stale: [] };

function runInsights(kind) {  // eslint-disable-line no-unused-vars
  var el = document.getElementById('insights-results');
  if (!el) return;
  el.style.display = '';
  el.textContent = 'Running ' + kind + ' analysis…';

  var staleDays = parseInt((document.getElementById('insights-stale-days')    || {}).value, 10) || 365;
  var diffDays  = parseInt((document.getElementById('insights-diff-days')     || {}).value, 10) || 7;
  var largeLim  = parseInt((document.getElementById('insights-largest-limit') || {}).value, 10) || 20;
  var url = '/api/insights/' + kind;
  if (kind === 'duplicates') url += '?threshold=0.90';
  if (kind === 'stale')      url += '?days=' + staleDays;
  if (kind === 'diff')       url += '?days=' + diffDays;
  if (kind === 'largest')    url += '?limit=' + largeLim;

  fetch(url)
    .then(function (r) { return r.json(); })
    .then(function (d) { renderInsightsResult(kind, d, el); })
    .catch(function (e) { el.textContent = 'Error: ' + e.message; });
}

function renderInsightsResult(kind, d, el) {
  if (kind === 'duplicates') {
    var clusters = d.clusters || [];
    _insightsLast.clusters = clusters;
    if (clusters.length === 0) {
      el.innerHTML = '<span style="color:var(--muted)">No duplicates found.</span>';
      return;
    }
    el.innerHTML = '<strong>' + clusters.length + ' duplicate cluster(s):</strong>'
      + '<div style="color:var(--muted);font-size:11px;margin-top:2px">approximate on large'
      + ' indexes — borderline pairs can be missed; exact-content groups are exhaustive</div>'
      + clusters.map(function (c, i) {
        return '<div style="margin-top:8px"><em>Cluster ' + (i + 1) + ' — similarity ' + (c.similarity * 100).toFixed(0) + '%</em>'
          + '<ul style="margin:2px 0 0 16px;padding:0">'
          + c.paths.map(function (p) { return '<li style="list-style:disc">' + escapeHtml(p) + '</li>'; }).join('')
          + '</ul>'
          + dismissEvidenceBtn('duplicate', i)
          + '</div>';
      }).join('');
  } else if (kind === 'stale') {
    var entries = d.entries || [];
    _insightsLast.stale = entries;
    if (entries.length === 0) {
      el.innerHTML = '<span style="color:var(--muted)">No stale projects found.</span>';
      return;
    }
    el.innerHTML = '<strong>' + entries.length + ' stale director(ies) (not modified in ' + (d.days || 365) + '+ days):</strong>'
      + '<table style="width:100%;border-collapse:collapse;margin-top:6px">'
      + entries.map(function (e, i) {
        return '<tr style="border-top:1px solid var(--border)">'
          + '<td style="padding:2px 6px;color:var(--muted)">' + e.days_since_modified + 'd</td>'
          + '<td style="padding:2px 6px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">' + escapeHtml(e.path) + '</td>'
          + '<td style="padding:2px 6px;text-align:right;white-space:nowrap">' + dismissEvidenceBtn('archive', i) + '</td>'
          + '</tr>';
      }).join('')
      + '</table>';
  } else if (kind === 'diff') {
    var added = d.added || [];
    var modified = d.modified || [];
    el.innerHTML = '<strong>Last ' + (d.days || 7) + ' day(s):</strong>'
      + '<div style="margin-top:6px"><em>Added (' + d.added_count + '):</em>'
      + (added.length === 0 ? ' <span style="color:var(--muted)">none</span>' : '<ul style="margin:2px 0 0 16px;padding:0">' + added.map(function (p) { return '<li style="list-style:circle;color:var(--green)">+ ' + escapeHtml(p) + '</li>'; }).join('') + '</ul>')
      + '</div><div style="margin-top:6px"><em>Modified (' + d.modified_count + '):</em>'
      + (modified.length === 0 ? ' <span style="color:var(--muted)">none</span>' : '<ul style="margin:2px 0 0 16px;padding:0">' + modified.map(function (p) { return '<li style="list-style:circle;color:var(--accent)">~ ' + escapeHtml(p) + '</li>'; }).join('') + '</ul>')
      + '</div>';
  } else if (kind === 'largest') {
    var rows = d.entries || [];
    if (rows.length === 0) {
      el.innerHTML = '<span style="color:var(--muted)">No indexed files found.</span>';
      return;
    }
    el.innerHTML = '<strong>Largest ' + rows.length + ' indexed file(s) by on-disk size:</strong>'
      + '<table style="width:100%;border-collapse:collapse;margin-top:6px">'
      + rows.map(function (e) {
        return '<tr style="border-top:1px solid var(--border)">'
          + '<td style="padding:2px 6px;color:var(--muted);text-align:right;white-space:nowrap">' + escapeHtml(insightsBytes(e.size)) + '</td>'
          + '<td style="padding:2px 6px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">' + escapeHtml(e.path) + '</td>'
          + '</tr>';
      }).join('')
      + '</table>';
  } else if (kind === 'languages') {
    var langs = d.languages || [];
    if (langs.length === 0) {
      el.innerHTML = '<span style="color:var(--muted)">No language-tagged chunks yet. Run <code>indexa deep</code> on source files first.</span>';
      return;
    }
    var total = d.total || 0;
    el.innerHTML = '<strong>Language breakdown (' + total + ' chunks):</strong>'
      + '<table style="width:100%;border-collapse:collapse;margin-top:6px">'
      + langs.map(function (l) {
        var pct = total > 0 ? (l.chunks / total * 100) : 0;
        return '<tr style="border-top:1px solid var(--border)">'
          + '<td style="padding:2px 6px;color:var(--muted);text-align:right;white-space:nowrap">' + pct.toFixed(1) + '%</td>'
          + '<td style="padding:2px 6px">' + escapeHtml(l.language) + '</td>'
          + '<td style="padding:2px 6px;text-align:right;color:var(--muted);white-space:nowrap">' + l.chunks + ' chunks</td>'
          + '</tr>';
      }).join('')
      + '</table>';
  }
}

/* Binary byte size (matches the server's human_bytes: 1 decimal, KB/MB/GB). */
function insightsBytes(n) {
  if (n >= 1073741824) return (n / 1073741824).toFixed(1) + ' GB';
  if (n >= 1048576) return (n / 1048576).toFixed(1) + ' MB';
  if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
  return n + ' B';
}

/* "Don't ask about this" button markup. The index is the only interpolated
   value (an integer from .map, never user input). */
function dismissEvidenceBtn(kind, idx) {
  return '<button class="btn-sm" onclick="dismissInsightEvidence(\'' + kind + '\',' + idx + ')"'
    + ' title="Record a dismissed decision — the question won&#39;t return unless its evidence changes">'
    + 'Don&#39;t ask about this</button>';
}

/* POST the evidence to the ledger as a pre-dismissed decision. The server
   recomputes the same fingerprint the detector would, so the future question
   is suppressed by sticky dismissal. */
function dismissInsightEvidence(kind, idx) {  // eslint-disable-line no-unused-vars
  var body;
  if (kind === 'duplicate') {
    var c = _insightsLast.clusters[idx];
    if (!c) return;
    // Paths only — the server re-derives the cluster from the detector's own
    // scan, so the dismissal fingerprint matches byte-for-byte.
    body = { kind: 'duplicate', paths: c.paths };
  } else {
    var e = _insightsLast.stale[idx];
    if (!e) return;
    body = { kind: 'archive', paths: [e.path] };
  }
  fetch('/api/review/dismiss-evidence', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
    .then(function (r) { return r.json().then(function (d) { return { ok: r.ok, d: d }; }); })
    .then(function (res) {
      if (!res.ok) { toast(res.d.error || 'Failed to record dismissal', 'error'); return; }
      // toast() escapes its message internally; nothing user-derived here anyway.
      toast(res.d.dismissed > 0
        ? 'Recorded — won’t ask again unless the evidence changes'
        : 'Nothing to dismiss (not detected at the asking threshold, or already dismissed)', 'info');
      loadReviewCount(); // an open question may have been dismissed with it
    })
    .catch(function (e2) { toast('Network error: ' + e2.message, 'error'); });
}
