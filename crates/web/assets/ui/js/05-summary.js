/* ── Summary view ── */
async function showSummary(path) {
  switchTab('tree');
  // Auto-scope Ask to whatever the user is now looking at (file-aware Ask). The
  // "Asking about: <name> ✕" chip lets them clear it for a whole-index question.
  if (typeof setAskScope === 'function') setAskScope(path);
  // Keep tree selection + deep-link URL in sync with what's being viewed (v0.37).
  selectedPath = path;
  if (typeof writeHash === 'function') writeHash();
  var view = document.getElementById('summary-view');
  view.style.display = 'block';
  view.innerHTML = '<div class="summary-pending">Loading summary…</div>';

  try {
    var r = await fetch('/api/summary?path=' + encodeURIComponent(path));
    // 404 = "no summary yet" (not an error — parse the body for pending state)
    if (!r.ok && r.status !== 404) throw new Error('Server error ' + r.status);
    var d = await r.json();

    if (d.error === 'no summary' || d.pending) {
      view.innerHTML = renderNoPendingSummary(path);
      // Wire both buttons here (this branch returns before the renderSummary wiring below).
      var nsEnq = view.querySelector('#enqueue-btn');
      if (nsEnq && typeof fireJob === 'function') {
        nsEnq.addEventListener('click', function() { fireJob('summarize', path); });
      }
      // Scoped Ask works on raw chunks even with no summary, so offer the bridge here too.
      var nsAsk = view.querySelector('#ask-cta-btn');
      if (nsAsk && typeof askAboutSelection === 'function') {
        nsAsk.addEventListener('click', function() { askAboutSelection(path); });
      }
      // A file can be previewed even with no summary (raw content is always available); the
      // endpoint returns a placeholder for directories, so this is safe regardless of kind.
      if (typeof showFilePreview === 'function') showFilePreview(path);
      return;
    }
    if (d.error) {
      view.innerHTML = '<div class="summary-pending" style="color:var(--red)">' + escapeHtml(d.error) + '</div>';
      if (typeof clearPreview === 'function') clearPreview();
      return;
    }

    view.innerHTML = renderSummary(d);

    view.querySelectorAll('.child-item[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    view.querySelectorAll('.crumb[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    var enqBtn = view.querySelector('#enqueue-btn');
    if (enqBtn) {
      // Fire the draining summarize job (same path as Regenerate) instead of the bare
      // /api/summarize enqueue, so items are actually processed — not just enqueued.
      enqBtn.addEventListener('click', function() {
        if (typeof fireJob === 'function') fireJob('summarize', path);
      });
    }
    // Regenerate button: triggers a new summarize job just like the row summarize action
    var regenBtn = view.querySelector('#regen-btn');
    if (regenBtn) {
      regenBtn.addEventListener('click', function() {
        if (typeof fireJob === 'function') fireJob('summarize', path);
      });
    }
    // "Ask about this file/folder" → switch to a scoped Ask for this selection.
    var askBtn = view.querySelector('#ask-cta-btn');
    if (askBtn && typeof askAboutSelection === 'function') {
      askBtn.addEventListener('click', function() { askAboutSelection(d.path); });
    }
    // Export menu items carry the format in data-export; the path is this summary's.
    view.querySelectorAll('.export-menu [data-export]').forEach(function(b) {
      b.addEventListener('click', function() { doExport(d.path, b.dataset.export); });
    });
    // Load smart label (classification) asynchronously after the summary renders
    if (typeof loadClassificationForPath === 'function') loadClassificationForPath(path);
    // Show the raw file beside the summary (files only; directories have no content to preview).
    if (typeof showFilePreview === 'function') {
      if (d.kind === 'file') showFilePreview(path);
      else if (typeof clearPreview === 'function') clearPreview();
    }
    // Append a collapsible "Indexed facts" panel (the web `indexa inspect`) below the summary.
    if (typeof appendInspectFacts === 'function') appendInspectFacts(view, path);
  } catch(e) {
    view.innerHTML = '<div class="summary-pending" style="color:var(--red)">Error: ' + escapeHtml(e.message) + '</div>';
    if (typeof clearPreview === 'function') clearPreview();
  }
}

function renderNoPendingSummary(path) {
  var name = path.split('/').pop() || path;
  return '<div class="summary-text">' +
    '<div style="color:var(--muted);margin-bottom:12px">No summary yet for <strong>' + escapeHtml(name) + '</strong>. ' +
    'You can still ask about it — answers use its raw content.</div>' +
    '<div class="summary-noctx-actions">' +
    '<button class="btn-sm summary-ask-btn" id="ask-cta-btn" title="Ask a question answered only from this file">' + ICO_CHAT + ' Ask about this file</button>' +
    '<button class="enqueue-btn" id="enqueue-btn" title="Queue an AI summary for this file">Generate summary</button>' +
    '</div></div>';
}

/* "Indexed facts" — the web `indexa inspect`. Appends a collapsible panel below the summary so the
   index is legible (what's stored for this path), not a black box. */
async function appendInspectFacts(view, path) {  // eslint-disable-line no-unused-vars
  if (!view) return;
  var details = document.createElement('details');
  details.className = 'inspect-facts';
  details.innerHTML = '<summary>Indexed facts</summary><div class="inspect-body">Loading…</div>';
  view.appendChild(details);
  var bodyEl = details.querySelector('.inspect-body');
  try {
    var r = await fetch('/api/inspect?path=' + encodeURIComponent(path));
    if (!r.ok) { bodyEl.textContent = 'unavailable'; return; }
    bodyEl.innerHTML = renderInspectFacts(await r.json());
  } catch (_) { bodyEl.textContent = 'unavailable'; }
}

function renderInspectFacts(d) {
  var rows = [];
  var row = function (k, v) {
    if (v !== null && v !== undefined && v !== '') {
      rows.push('<div class="if-row"><span class="if-k">' + k + '</span><span class="if-v">' + v + '</span></div>');
    }
  };
  if (d.kind) row('Kind', escapeHtml(d.kind));
  if (typeof d.size === 'number') {
    var kb = d.size / 1024;
    row('Size', kb < 1024 ? kb.toFixed(1) + ' KB' : (kb / 1024).toFixed(1) + ' MB');
  }
  if (d.modified_s) row('Modified', fmtRelTime(d.modified_s));
  row('Chunks', d.chunk_count + (d.language ? ' (' + escapeHtml(d.language) + ')' : ''));
  row('Summary', d.has_summary ? ('yes — ' + escapeHtml(d.summary_model || '')) : 'none');
  if (d.category) {
    row('Category', escapeHtml(d.category) + (typeof d.confidence === 'number' ? ' (' + Math.round(d.confidence * 100) + '%)' : ''));
  }
  if (d.apps && d.apps.length) {
    var primary = d.apps.filter(function (a) { return a.is_primary; })[0] || d.apps[0];
    var others = d.apps.filter(function (a) { return a.kind !== primary.kind; })
      .map(function (a) { return a.name; });
    var appVal = escapeHtml(primary.name) + ' (' + escapeHtml(primary.family) + ')' +
      (others.length ? ' · also ' + escapeHtml(others.join(', ')) : '');
    row('App', appVal);
  }
  if (typeof d.weight === 'number' && Math.abs(d.weight - 1) > 0.001) row('Weight', d.weight.toFixed(2));
  if (d.imports || d.defines || d.calls) {
    row('Graph', d.imports + ' imports · ' + d.defines + ' defines · ' + d.calls + ' calls');
  }
  return '<div class="inspect-rows">' + rows.join('') + '</div>' +
    '<div class="if-note">Derived from your files — re-derivable by re-indexing; sources are never modified.</div>';
}

/* Format a unix timestamp as a relative human string: "just now", "3 min ago",
   "2 hr ago", "5 days ago". Falls back to locale date for older timestamps. */
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
  var iconInner = d.kind === 'dir'
    ? '<path d="M3 7a2 2 0 0 1 2-2h4l2 2.5h8a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>'
    : '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><polyline points="14 3 14 8 19 8"/>';
  var icon = '<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' + iconInner + '</svg>';

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
        var cIcon = c.kind === 'dir' ? ICO_FOLDER : ICO_FILE;
        return '<div class="child-item" data-path="' + escapeAttr(c.path) + '">' +
          '<div class="child-row"><span>' + cIcon + '</span><span class="child-name">' + escapeHtml(c.name) + '</span></div>' +
          '<div class="child-summary">' + escapeHtml(c.summary) + '</div>' +
          '</div>';
      }).join('') + '</div>';
  }

  // Freshness: relative time + model name
  var relTime = d.generated_at ? fmtRelTime(d.generated_at) : '';
  var metaParts = [];
  if (d.model)  metaParts.push(escapeHtml(d.model));
  if (relTime)  metaParts.push(relTime);
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

  // Header toolbar: a primary "Ask about this …" CTA (the bridge into scoped Ask),
  // then Regenerate + Export.
  var askLabel = d.kind === 'dir' ? 'Ask about this folder' : 'Ask about this file';
  var askCtaHtml = '<button class="btn-sm summary-ask-btn" id="ask-cta-btn" ' +
    'title="Ask a question answered only from ' + escapeAttr(d.path) + '">' + ICO_CHAT + ' ' + escapeHtml(askLabel) + '</button>';

  var regenHtml = '<button class="btn-sm summary-regen-btn" id="regen-btn" ' +
    'title="Re-run AI summarization for this path" aria-label="Regenerate summary">↻ Regenerate</button>';

  // Export menu items carry the format in data-export; showSummary() wires the clicks
  // to doExport(d.path, fmt) — avoids fragile path interpolation inside an onclick attribute.
  var exportBtnHtml =
    '<div class="export-menu-wrap">' +
    '<button class="btn-sm export-menu-btn" title="Export context as XML, Markdown, or JSON" aria-label="Export context" aria-haspopup="menu" aria-expanded="false" onclick="toggleExportMenu(this)">Export ↓</button>' +
    '<div class="export-menu" role="menu" hidden>' +
    '<button role="menuitem" data-export="xml">XML <span class="export-hint">for Claude / Cursor</span></button>' +
    '<button role="menuitem" data-export="md">Markdown</button>' +
    '<button role="menuitem" data-export="json">JSON</button>' +
    // Optional relational slice — leave blank to export everything. Applied to the chosen format above.
    '<div class="export-slice">' +
    '<input id="export-since" class="export-slice-in" placeholder="changed since — e.g. 7d, 12h" aria-label="Export only files changed within this window (e.g. 7d, 12h, 90m)">' +
    '<input id="export-cat" class="export-slice-in" placeholder="category — e.g. code" aria-label="Export only files in this classification category">' +
    '</div>' +
    '</div></div>';

  return crumbHtml +
    '<div class="summary-header"><span style="font-size:22px">' + icon + '</span>' +
    '<span class="summary-title">' + escapeHtml(name) + '</span>' + covChip +
    '<span style="flex:1"></span>' + askCtaHtml + regenHtml + exportBtnHtml + '</div>' +
    metaHtml +
    abstractHtml +
    '<div class="summary-text">' + escapeHtml(d.summary) + '</div>' +
    childrenHtml +
    // Smart-label container: populated asynchronously by loadClassificationForPath()
    '<div id="summary-classify"></div>';
}

function toggleExportMenu(btn) {
  if (!btn) return;
  var menu = btn.nextElementSibling;
  if (!menu) return;
  var isHidden = menu.hidden;
  // Collapse any other open export menus and reset their buttons' aria-expanded.
  document.querySelectorAll('.export-menu').forEach(function(m) {
    m.hidden = true;
    var b = m.previousElementSibling;
    if (b) b.setAttribute('aria-expanded', 'false');
  });
  menu.hidden = !isHidden;
  btn.setAttribute('aria-expanded', menu.hidden ? 'false' : 'true');
  if (!menu.hidden) {
    setTimeout(function() {
      document.addEventListener('click', function closeMenu(e) {
        if (!menu.contains(e.target) && e.target !== btn) {
          menu.hidden = true;
          btn.setAttribute('aria-expanded', 'false');
          document.removeEventListener('click', closeMenu);
        }
      });
    }, 0);
  }
}

function doExport(path, format) {
  if (!path || typeof path !== 'string') {
    toast('Select a folder first', 'warn');
    document.querySelectorAll('.export-menu').forEach(function(m) { m.hidden = true; });
    return;
  }
  // Brief disabled state on all export buttons to prevent double-clicks and give visual feedback.
  var exportBtns = document.querySelectorAll('.export-menu-btn, .export-menu button');
  exportBtns.forEach(function(b) { b.disabled = true; });
  setTimeout(function() { exportBtns.forEach(function(b) { b.disabled = false; }); }, 800);

  var url = '/api/export?format=' + encodeURIComponent(format) + '&path=' + encodeURIComponent(path);
  // Optional relational slice (v0.60): append the filters when the user filled them in.
  var since = (document.getElementById('export-since') || {}).value;
  var cat = (document.getElementById('export-cat') || {}).value;
  if (since && since.trim()) url += '&changed_since=' + encodeURIComponent(since.trim());
  if (cat && cat.trim()) url += '&category=' + encodeURIComponent(cat.trim());
  window.open(url, '_blank');
  document.querySelectorAll('.export-menu').forEach(function(m) { m.hidden = true; });
}
