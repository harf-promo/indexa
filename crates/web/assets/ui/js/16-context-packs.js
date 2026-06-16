// ── Context Packs ─────────────────────────────────────────────────────────────
//
// Manages the #section-packs settings panel: list, create, add/remove paths,
// export. Loaded once on DOMContentLoaded; refreshes on drawer open.

var _currentPackName = null;

document.addEventListener('DOMContentLoaded', function () {
  loadPacks();
});

// Called by the settings drawer open event (wired in 01-state-theme-tabs.js via
// the existing openDrawer hook that calls loadSettings()).
function loadPacks() {  // eslint-disable-line no-unused-vars
  fetch('/api/packs')
    .then(function (r) { return r.json(); })
    .then(renderPackList)
    .catch(function () { /* silently ignore — drawer may open before server ready */ });
}

function renderPackList(packs) {
  var list  = document.getElementById('packs-list');
  var empty = document.getElementById('packs-empty-msg');
  if (!list) return;
  if (!packs || packs.length === 0) {
    list.innerHTML = '';
    if (empty) empty.style.display = '';
    return;
  }
  if (empty) empty.style.display = 'none';
  list.innerHTML = packs.map(function (p) {
    var desc = p.description ? ' <span style="color:var(--muted);font-size:11px">— ' + escapeHtml(p.description) + '</span>' : '';
    return '<div class="key-row" style="justify-content:space-between;flex-wrap:wrap;gap:4px" data-pack-name="' + escapeHtml(p.name) + '">'
      + '<span style="font-size:13px"><strong>' + escapeHtml(p.name) + '</strong>' + desc + ' <span style="color:var(--muted);font-size:11px">(' + p.path_count + ' path' + (p.path_count === 1 ? '' : 's') + ')</span></span>'
      + '<span style="display:flex;gap:4px">'
      + '<button class="btn-sm" style="font-size:11px" onclick="openPackEditor(' + JSON.stringify(p.name) + ')">Edit</button>'
      + '<button class="btn-sm" style="font-size:11px" onclick="quickExportPack(' + JSON.stringify(p.name) + ')">Export</button>'
      + '<button class="btn-sm btn-danger" style="font-size:11px" onclick="deletePack(' + JSON.stringify(p.name) + ')">Delete</button>'
      + '</span>'
      + '</div>';
  }).join('');
}

function createPack() {  // eslint-disable-line no-unused-vars
  var nameEl   = document.getElementById('pack-new-name');
  var descEl   = document.getElementById('pack-new-desc');
  var statusEl = document.getElementById('pack-create-status');
  var name = (nameEl ? nameEl.value : '').trim();
  if (!name) { if (statusEl) statusEl.textContent = 'Pack name is required.'; return; }
  fetch('/api/packs', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: name, description: (descEl ? descEl.value.trim() : '') || null }),
  })
    .then(function (r) { return r.json().then(function (d) { return { ok: r.ok, d: d }; }); })
    .then(function (res) {
      if (!res.ok) {
        if (statusEl) { statusEl.textContent = res.d.error || 'Error creating pack.'; }
        return;
      }
      if (nameEl) nameEl.value = '';
      if (descEl) descEl.value = '';
      if (statusEl) statusEl.textContent = '';
      loadPacks();
    })
    .catch(function (e) { if (statusEl) statusEl.textContent = 'Error: ' + e.message; });
}

function deletePack(name) {  // eslint-disable-line no-unused-vars
  fetch('/api/packs/' + encodeURIComponent(name), { method: 'DELETE' })
    .then(function () { loadPacks(); closePackEditor(); });
}

function openPackEditor(name) {  // eslint-disable-line no-unused-vars
  _currentPackName = name;
  var editor  = document.getElementById('pack-path-editor');
  var heading = document.getElementById('pack-editor-name');
  if (!editor || !heading) return;
  heading.textContent = name;
  editor.style.display = '';
  refreshPackEditorPaths(name);
}

function closePackEditor() {  // eslint-disable-line no-unused-vars
  _currentPackName = null;
  var editor = document.getElementById('pack-path-editor');
  if (editor) editor.style.display = 'none';
}

function refreshPackEditorPaths(name) {
  fetch('/api/packs/' + encodeURIComponent(name) + '/paths')
    .then(function (r) { return r.json(); })
    .then(function (d) {
      var container = document.getElementById('pack-editor-paths');
      if (!container) return;
      var paths = d.paths || [];
      if (paths.length === 0) {
        container.innerHTML = '<span style="color:var(--muted);font-size:12px">No paths yet — add one below.</span>';
        return;
      }
      container.innerHTML = paths.map(function (p) {
        return '<div style="display:flex;justify-content:space-between;align-items:center;font-size:12px;gap:4px">'
          + '<span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;flex:1" title="' + escapeHtml(p) + '">' + escapeHtml(p) + '</span>'
          + '<button class="btn-sm btn-danger" style="font-size:10px;padding:1px 6px;flex-shrink:0" onclick="removePackPath(' + JSON.stringify(p) + ')">✕</button>'
          + '</div>';
      }).join('');
    })
    .catch(function () {});
}

function addPackPath() {  // eslint-disable-line no-unused-vars
  if (!_currentPackName) return;
  var input    = document.getElementById('pack-add-path');
  var statusEl = document.getElementById('pack-path-status');
  var path = (input ? input.value : '').trim();
  if (!path) return;
  fetch('/api/packs/' + encodeURIComponent(_currentPackName) + '/paths', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ paths: [path] }),
  })
    .then(function (r) { return r.json().then(function (d) { return { ok: r.ok, d: d }; }); })
    .then(function (res) {
      if (!res.ok) {
        if (statusEl) statusEl.textContent = res.d.error || 'Error adding path.';
        return;
      }
      if (input) input.value = '';
      if (statusEl) statusEl.textContent = '';
      refreshPackEditorPaths(_currentPackName);
      loadPacks();
    })
    .catch(function (e) { if (statusEl) statusEl.textContent = 'Error: ' + e.message; });
}

function removePackPath(path) {  // eslint-disable-line no-unused-vars
  if (!_currentPackName) return;
  fetch('/api/packs/' + encodeURIComponent(_currentPackName) + '/paths', {
    method: 'DELETE',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ paths: [path] }),
  })
    .then(function () {
      refreshPackEditorPaths(_currentPackName);
      loadPacks();
    });
}

function exportCurrentPack() {  // eslint-disable-line no-unused-vars
  if (!_currentPackName) return;
  var fmtEl  = document.getElementById('pack-export-format');
  var format = fmtEl ? fmtEl.value : 'xml';
  doExportPack(_currentPackName, format);
}

function quickExportPack(name) {  // eslint-disable-line no-unused-vars
  doExportPack(name, 'xml');
}

function doExportPack(name, format) {
  var url = '/api/packs/' + encodeURIComponent(name) + '/export?format=' + encodeURIComponent(format);
  fetch(url)
    .then(function (r) {
      if (!r.ok) {
        return r.json().then(function (d) { throw new Error(d.error || 'Export failed.'); });
      }
      return r.text();
    })
    .then(function (text) {
      var ext = format === 'json' ? 'json' : format === 'md' ? 'md' : 'xml';
      var blob = new Blob([text], { type: 'text/plain' });
      var a    = document.createElement('a');
      a.href   = URL.createObjectURL(blob);
      a.download = name.replace(/[^a-z0-9_-]/gi, '_') + '.' + ext;
      a.click();
      URL.revokeObjectURL(a.href);
    })
    .catch(function (e) {
      var statusEl = document.getElementById('pack-path-status');
      if (statusEl) { statusEl.textContent = e.message; }
    });
}

function searchCurrentPack() {  // eslint-disable-line no-unused-vars
  if (!_currentPackName) return;
  var input     = document.getElementById('pack-search-query');
  var resultsEl = document.getElementById('pack-search-results');
  var q = (input ? input.value : '').trim();
  if (!q || !resultsEl) return;
  resultsEl.style.display = '';
  resultsEl.textContent = 'Searching…';
  fetch('/api/packs/' + encodeURIComponent(_currentPackName) + '/search?q=' + encodeURIComponent(q) + '&limit=10')
    .then(function (r) { return r.json(); })
    .then(function (d) {
      var hits = d.hits || [];
      if (hits.length === 0) {
        resultsEl.textContent = 'No results.';
        return;
      }
      resultsEl.innerHTML = hits.map(function (h) {
        var heading = h.heading ? ' <span style="color:var(--muted)">[' + escapeHtml(h.heading) + ']</span>' : '';
        return '<div style="margin-bottom:6px"><strong style="color:var(--accent)">' + escapeHtml(h.path) + '</strong>' + heading
          + '<div style="color:var(--muted);margin-top:2px">' + escapeHtml(h.snippet.slice(0, 160)) + '</div></div>';
      }).join('');
    })
    .catch(function (e) { resultsEl.textContent = 'Error: ' + e.message; });
}
