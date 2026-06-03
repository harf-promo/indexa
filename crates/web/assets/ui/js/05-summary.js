/* ── Summary view ── */
async function showSummary(path) {
  switchTab('tree');
  const view = document.getElementById('summary-view');
  view.style.display = 'block';
  view.innerHTML = '<div class="summary-pending">Loading summary…</div>';

  try {
    const r = await fetch('/api/summary?path=' + encodeURIComponent(path));
    const d = await r.json();

    if (d.error === 'no summary' || d.pending) {
      view.innerHTML = renderNoPendingSummary(path);
      return;
    }
    if (d.error) {
      view.innerHTML = '<div class="summary-pending" style="color:var(--red)">' + escapeHtml(d.error) + '</div>';
      return;
    }

    view.innerHTML = renderSummary(d);

    view.querySelectorAll('.child-item[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    view.querySelectorAll('.crumb[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    const enqBtn = view.querySelector('#enqueue-btn');
    if (enqBtn) {
      enqBtn.addEventListener('click', async function() {
        enqBtn.disabled = true;
        enqBtn.textContent = 'Queued…';
        await fetch('/api/summarize?path=' + encodeURIComponent(path), { method: 'POST' });
        setTimeout(function() { showSummary(path); }, 2000);
      });
    }
    // Regenerate button: triggers a new summarize job just like the row ⚡ action
    const regenBtn = view.querySelector('#regen-btn');
    if (regenBtn) {
      regenBtn.addEventListener('click', function() {
        if (typeof fireJob === 'function') fireJob('summarize', path);
      });
    }
  } catch(e) {
    view.innerHTML = '<div class="summary-pending" style="color:var(--red)">Error: ' + escapeHtml(e.message) + '</div>';
  }
}

function renderNoPendingSummary(path) {
  const name = path.split('/').pop() || path;
  return '<div class="summary-text">' +
    '<div style="color:var(--muted);margin-bottom:12px">No summary yet for <strong>' + escapeHtml(name) + '</strong></div>' +
    '<button class="enqueue-btn" id="enqueue-btn">Generate summary</button>' +
    '</div>';
}

/* Format a unix timestamp as a relative human string: "just now", "3 minutes ago",
   "2 hours ago", "5 days ago". Falls back to locale date string for older dates. */
function fmtRelTime(unixSecs) {
  if (!unixSecs) return '';
  var now = Math.floor(Date.now() / 1000);
  var diff = now - unixSecs;
  if (diff < 60)   return 'just now';
  if (diff < 3600) return Math.floor(diff / 60) + ' min ago';
  if (diff < 86400) return Math.floor(diff / 3600) + ' hr ago';
  if (diff < 86400 * 30) return Math.floor(diff / 86400) + ' days ago';
  return new Date(unixSecs * 1000).toLocaleDateString();
}

function renderSummary(d) {
  var name = d.path.split('/').pop() || d.path;
  var icon = d.kind === 'dir' ? '📁' : '📄';

  var crumbHtml = '';
  if (d.crumbs && d.crumbs.length) {
    crumbHtml = '<div class="crumbs">' +
      d.crumbs.map(function(c) {
        return '<a class="crumb" data-path="' + escapeAttr(c.path) + '">' + escapeHtml(c.name) + '</a>';
      }).join('<span class="sep">›</span>') +
      '<span class="sep">›</span><span>' + escapeHtml(name) + '</span></div>';
  }

  var childrenHtml = '';
  if (d.children && d.children.length) {
    childrenHtml = '<div class="children-section"><h3>Contents (' + d.children.length + ')</h3>' +
      d.children.map(function(c) {
        var cIcon = c.kind === 'dir' ? '📁' : '📄';
        return '<div class="child-item" data-path="' + escapeAttr(c.path) + '">' +
          '<div class="child-row"><span>' + cIcon + '</span><span class="child-name">' + escapeHtml(c.name) + '</span></div>' +
          '<div class="child-summary">' + escapeHtml(c.summary) + '</div>' +
          '</div>';
      }).join('') + '</div>';
  }

  // Freshness: relative time + model name
  var relTime = d.generated_at ? fmtRelTime(d.generated_at) : '';
  var metaParts = [];
  if (d.model)   metaParts.push(escapeHtml(d.model));
  if (relTime)   metaParts.push(relTime);
  var metaHtml = metaParts.length
    ? '<div class="summary-meta">' + metaParts.join(' \xb7 ') + '</div>'
    : '';

  // Subtree context-coverage chip
  var cov = coverageByPath[d.path];
  var covChip = '';
  if (cov && cov.total > 0) {
    var pct = Math.round(100 * cov.covered / cov.total);
    covChip = '<span class="cov-chip" title="' + cov.covered + ' of ' + cov.total +
      ' folders in this subtree have context built">context: ' + pct + '%</span>';
  }

  // L0 one-liner abstract (returned by /api/summary as abstract_ but previously unused)
  var abstractHtml = '';
  if (d.abstract_ && d.abstract_.trim()) {
    abstractHtml = '<div class="summary-abstract">' + escapeHtml(d.abstract_) + '</div>';
  }

  // Regenerate button — runs a new summarize job, same as the row 📝 action
  var regenHtml = '<button class="btn-sm summary-regen-btn" id="regen-btn" ' +
    'title="Re-run AI summarization for this path" aria-label="Regenerate summary">↻ Regenerate</button>';

  return crumbHtml +
    '<div class="summary-header"><span style="font-size:22px">' + icon + '</span>' +
    '<span class="summary-title">' + escapeHtml(name) + '</span>' + covChip +
    '<span style="flex:1"></span>' + regenHtml + '</div>' +
    metaHtml +
    abstractHtml +
    '<div class="summary-text">' + escapeHtml(d.summary) + '</div>' +
    childrenHtml;
}
