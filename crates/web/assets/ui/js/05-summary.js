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

function renderSummary(d) {
  const name = d.path.split('/').pop() || d.path;
  const icon = d.kind === 'dir' ? '📁' : '📄';

  let crumbHtml = '';
  if (d.crumbs && d.crumbs.length) {
    crumbHtml = '<div class="crumbs">' +
      d.crumbs.map(function(c) {
        return '<a class="crumb" data-path="' + escapeAttr(c.path) + '">' + escapeHtml(c.name) + '</a>';
      }).join('<span class="sep">›</span>') +
      '<span class="sep">›</span><span>' + escapeHtml(name) + '</span></div>';
  }

  let childrenHtml = '';
  if (d.children && d.children.length) {
    childrenHtml = '<div class="children-section"><h3>Contents (' + d.children.length + ')</h3>' +
      d.children.map(function(c) {
        const cIcon = c.kind === 'dir' ? '📁' : '📄';
        return '<div class="child-item" data-path="' + escapeAttr(c.path) + '">' +
          '<div class="child-row"><span>' + cIcon + '</span><span class="child-name">' + escapeHtml(c.name) + '</span></div>' +
          '<div class="child-summary">' + escapeHtml(c.summary) + '</div>' +
          '</div>';
      }).join('') + '</div>';
  }

  const ts = d.generated_at ? new Date(d.generated_at * 1000).toLocaleDateString() : '';
  // Subtree context-coverage chip, from the rollup stashed when the tree row was built.
  // Absent for paths never rendered in the tree (e.g. deep breadcrumb nav) — graceful.
  const cov = coverageByPath[d.path];
  let covChip = '';
  if (cov && cov.total > 0) {
    const pct = Math.round(100 * cov.covered / cov.total);
    covChip = '<span class="cov-chip" title="' + cov.covered + ' of ' + cov.total +
      ' folders in this subtree have context built">context: ' + pct + '%</span>';
  }
  const exportBtnHtml =
    '<div class="export-menu-wrap">' +
    '<button class="btn-sm export-menu-btn" title="Export context as XML, Markdown, or JSON" aria-label="Export context" onclick="toggleExportMenu(this)">Export ↓</button>' +
    '<div class="export-menu" hidden>' +
    '<button onclick="doExport(' + JSON.stringify(d.path) + ',\'xml\')">XML <span class="export-hint">for Claude / Cursor</span></button>' +
    '<button onclick="doExport(' + JSON.stringify(d.path) + ',\'md\')">Markdown</button>' +
    '<button onclick="doExport(' + JSON.stringify(d.path) + ',\'json\')">JSON</button>' +
    '</div></div>';

  return crumbHtml +
    '<div class="summary-header"><span style="font-size:22px">' + icon + '</span>' +
    '<span class="summary-title">' + escapeHtml(name) + '</span>' + covChip +
    '<span style="flex:1"></span>' + exportBtnHtml + '</div>' +
    '<div class="summary-meta">Model: ' + escapeHtml(d.model) + (ts ? ' \xb7 ' + ts : '') + '</div>' +
    '<div class="summary-text">' + escapeHtml(d.summary) + '</div>' +
    childrenHtml;
}

function toggleExportMenu(btn) {
  var menu = btn.nextElementSibling;
  if (!menu) return;
  var isHidden = menu.hidden;
  // Close any other open export menus first
  document.querySelectorAll('.export-menu').forEach(function(m) { m.hidden = true; });
  menu.hidden = !isHidden;
  if (!menu.hidden) {
    // Close on outside click
    setTimeout(function() {
      document.addEventListener('click', function closeMenu(e) {
        if (!menu.contains(e.target) && e.target !== btn) {
          menu.hidden = true;
          document.removeEventListener('click', closeMenu);
        }
      });
    }, 0);
  }
}

function doExport(path, format) {
  var url = '/api/export?format=' + encodeURIComponent(format) + '&path=' + encodeURIComponent(path);
  window.open(url, '_blank');
  document.querySelectorAll('.export-menu').forEach(function(m) { m.hidden = true; });
}

