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
    .catch(function () {
      var list = document.getElementById('packs-list');
      if (list) list.innerHTML = '<span style="color:var(--muted);font-size:12px">Couldn\'t load — is the server running?</span>';
    });
}

// Security note (stored-XSS fix): `p.name` is a pack name taken raw from `POST /api/packs`
// with no charset validation server-side, so it must never be interpolated into an HTML
// attribute (the previous code built each row's Edit/Export/Delete handlers by
// concatenating JSON.stringify(p.name) straight into the attribute string, which let a
// hostile pack name break out of the attribute and inject live markup). The row buttons
// below carry no identity in their markup at all; they're wired up afterward via
// addEventListener with the real `p` object captured in a closure, so the untrusted value
// never touches HTML. (`data-pack-name` above is still HTML-escaped via escapeHtml, same
// as it always was — it's a plain attribute value, not part of a handler string.)
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
      + '<button class="btn-sm pack-edit-btn" style="font-size:11px" type="button">Edit</button>'
      + '<button class="btn-sm pack-refresh-btn" style="font-size:11px" type="button" title="Reindex stale members">Refresh</button>'
      + '<button class="btn-sm pack-export-btn" style="font-size:11px" type="button">Export</button>'
      + '<button class="btn-sm btn-danger pack-delete-btn" style="font-size:11px" type="button">Delete</button>'
      + '</span>'
      + '</div>';
  }).join('');
  var rows = list.children;
  packs.forEach(function (p, i) {
    var row = rows[i];
    if (!row) return;
    var editBtn = row.querySelector('.pack-edit-btn');
    if (editBtn) editBtn.addEventListener('click', function () { openPackEditor(p.name); });
    var refreshBtn = row.querySelector('.pack-refresh-btn');
    if (refreshBtn) refreshBtn.addEventListener('click', function () { refreshPack(p.name); });
    var exportBtn = row.querySelector('.pack-export-btn');
    if (exportBtn) exportBtn.addEventListener('click', function () { quickExportPack(p.name); });
    var deleteBtn = row.querySelector('.pack-delete-btn');
    if (deleteBtn) deleteBtn.addEventListener('click', function () { deletePack(p.name); });
  });
}

/* Reindex a pack's stale members (files changed on disk since last indexed, or deleted).
   POST /api/packs/:name/refresh starts a background job (subscribed via the existing global
   `subscribeJob`, same as every other job-starting action) unless nothing is stale, in which
   case it responds synchronously with { stale_files: 0 } and no job_id. Security note: no pack
   name is ever written into an HTML attribute here — same stored-XSS guard as the rest of this
   file — `name` only ever reaches fetch()'s URL (encodeURIComponent'd) and toast() text. */
function refreshPack(name) {  // eslint-disable-line no-unused-vars
  fetch('/api/packs/' + encodeURIComponent(name) + '/refresh', { method: 'POST' })
    .then(function (r) {
      if (r.status === 429) {
        return r.json().then(function (d) {
          throw new Error(d.error || 'Another job is already running — try again shortly.');
        });
      }
      if (!r.ok) {
        return r.json().then(function (d) { throw new Error(d.error || 'Refresh failed.'); });
      }
      return r.json();
    })
    .then(function (d) {
      if (d.job_id) {
        subscribeJob(d.job_id, name, 'pack_refresh');
        if (typeof toast === 'function') {
          toast('Refreshing pack “' + name + '”…', 'info', {
            label: 'Watch progress',
            onClick: function () { if (typeof switchTab === 'function') switchTab('jobs'); },
          });
        }
      } else if (typeof toast === 'function') {
        toast('Pack “' + name + '” has no stale files.', 'info');
      }
    })
    .catch(function (e) {
      if (typeof toast === 'function') toast('Pack refresh failed: ' + e.message, 'error');
    });
}

function createPack() {  // eslint-disable-line no-unused-vars
  var nameEl   = document.getElementById('pack-new-name');
  var descEl   = document.getElementById('pack-new-desc');
  var statusEl = document.getElementById('pack-create-status');
  var btn = document.querySelector('button[onclick="createPack()"]');
  var name = (nameEl ? nameEl.value : '').trim();
  if (!name) { if (statusEl) statusEl.textContent = 'Pack name is required.'; return; }
  if (btn) btn.disabled = true;
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
    .catch(function (e) { if (statusEl) statusEl.textContent = 'Error: ' + e.message; })
    .finally(function () { if (btn) btn.disabled = false; });
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

// Security note (stored-XSS fix): `p` (a pack member path) must never be interpolated into
// an HTML attribute — same class of bug as renderPackList above (previously each remove
// button's handler was built by concatenating JSON.stringify(p) straight into the attribute
// string). The remove buttons below carry no identity in their markup; they're wired up
// afterward via addEventListener with the real path captured in a closure.
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
          + '<button class="btn-sm btn-danger pack-remove-path-btn" style="font-size:10px;padding:1px 6px;flex-shrink:0" type="button" title="Remove path" aria-label="Remove path">✕</button>'
          + '</div>';
      }).join('');
      var rows = container.children;
      paths.forEach(function (p, i) {
        var btn = rows[i] && rows[i].querySelector('.pack-remove-path-btn');
        if (btn) btn.addEventListener('click', function () { removePackPath(p); });
      });
    })
    .catch(function () {});
}

function addPackPath() {  // eslint-disable-line no-unused-vars
  if (!_currentPackName) return;
  var input    = document.getElementById('pack-add-path');
  var statusEl = document.getElementById('pack-path-status');
  var btn = document.querySelector('button[onclick="addPackPath()"]');
  var path = (input ? input.value : '').trim();
  if (!path) return;
  if (btn) btn.disabled = true;
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
    .catch(function (e) { if (statusEl) statusEl.textContent = 'Error: ' + e.message; })
    .finally(function () { if (btn) btn.disabled = false; });
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
  document.querySelectorAll('.export-menu').forEach(function (m) { m.hidden = true; });
}

/* Inject named packs into the toolbar Export menu (Wave 3). */
function fillExportPacks(menu) {
  var list = document.getElementById('export-pack-list');
  var sep = document.getElementById('export-pack-sep');
  if (!list) return;
  list.textContent = '';
  fetch('/api/packs')
    .then(function (r) { return r.json(); })
    .then(function (packs) {
      if (!Array.isArray(packs) || !packs.length) {
        if (sep) sep.hidden = true;
        return;
      }
      if (sep) sep.hidden = false;
      packs.forEach(function (p) {
        var b = document.createElement('button');
        b.type = 'button';
        b.textContent = 'Pack: ' + p.name;
        b.title = (p.path_count || 0) + ' path' + (p.path_count === 1 ? '' : 's');
        b.addEventListener('click', function () { quickExportPack(p.name); });
        list.appendChild(b);
      });
    })
    .catch(function () { if (sep) sep.hidden = true; });
}

/* "<parent>/<basename>" for a path — a collision-safer pack name than the bare basename
   alone (mirrors the intent behind projects.rs's project_display_name, which qualifies
   generic names like "admin"/"mobile"/"src" the same way). `null` when there's no parent
   segment to qualify with. */
function parentQualifiedName(path) {
  var parts = path.split('/').filter(Boolean);
  if (parts.length < 2) return null;
  return parts[parts.length - 2] + '/' + parts[parts.length - 1];
}

function createPackAndAddPath(name, path) {
  return fetch('/api/packs', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: name, description: path }),
  }).then(function (r) {
    return r.json().then(function (d) { return { status: r.status, ok: r.ok, d: d }; });
  });
}

/* Create a pack named after the selected folder and add that path. The backend correctly
   409s on a name collision (bare basenames like "admin"/"src" collide across projects), but
   until now the JS had no recovery from that beyond an error toast with no path forward —
   retry once with a parent-qualified name before giving up. */
function newPackFromSelection() {  // eslint-disable-line no-unused-vars
  var path = (typeof selectedPath === 'string') ? selectedPath : '';
  if (!path) {
    if (typeof toast === 'function') toast('Select a folder first', 'warn');
    return;
  }
  var name = path.split('/').filter(Boolean).pop() || 'pack';

  createPackAndAddPath(name, path)
    .then(function (res) {
      if (res.ok) return { name: name };
      if (res.status !== 409) throw new Error(res.d.error || 'Could not create pack');
      var qualified = parentQualifiedName(path);
      if (!qualified || qualified === name) {
        throw new Error(
          'A pack named "' + name + '" already exists — rename it from Settings → Context Packs.'
        );
      }
      return createPackAndAddPath(qualified, path).then(function (res2) {
        if (res2.ok) return { name: qualified };
        throw new Error(
          res2.status === 409
            ? 'Packs named "' + name + '" and "' + qualified + '" both already exist — ' +
              'rename one from Settings → Context Packs.'
            : (res2.d.error || 'Could not create pack')
        );
      });
    })
    .then(function (created) {
      return fetch('/api/packs/' + encodeURIComponent(created.name) + '/paths', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ paths: [path] }),
      }).then(function (r) {
        if (!r.ok) throw new Error('Pack created but adding the path failed');
        if (typeof toast === 'function') toast('Pack “' + created.name + '” created from selection', 'info');
        if (typeof loadPacks === 'function') loadPacks();
      });
    })
    .catch(function (e) {
      if (typeof toast === 'function') toast(e.message || 'Pack error', 'error');
    });
  document.querySelectorAll('.export-menu').forEach(function (m) { m.hidden = true; });
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
      // Unconditional toast, not just the in-drawer #pack-path-status span: this function is
      // also reached from the toolbar Export menu's quickExportPack(), where the Settings
      // drawer that span lives in is closed — the error used to be written where nobody
      // could see it, reading as "nothing happened" on export failure.
      if (typeof toast === 'function') toast('Pack export failed: ' + e.message, 'error');
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
