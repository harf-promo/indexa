/* ── Summary view ── */
async function showSummary(path) {
  switchTab('tree');
  // Auto-scope Ask to whatever the user is now looking at (file-aware Ask). The
  // "Asking about: <name> ✕" chip lets them clear it for a whole-index question.
  if (typeof setAskScope === 'function') setAskScope(path);
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
    // Regenerate button: triggers a new summarize job just like the row 📝 action
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
    '<button class="btn-sm summary-ask-btn" id="ask-cta-btn" title="Ask a question answered only from this file">&#x1F4AC; Ask about this file</button>' +
    '<button class="enqueue-btn" id="enqueue-btn">Generate summary</button>' +
    '</div></div>';
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
    'title="Ask a question answered only from ' + escapeAttr(d.path) + '">&#x1F4AC; ' + escapeHtml(askLabel) + '</button>';

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
  var url = '/api/export?format=' + encodeURIComponent(format) + '&path=' + encodeURIComponent(path);
  window.open(url, '_blank');
  document.querySelectorAll('.export-menu').forEach(function(m) { m.hidden = true; });
}
