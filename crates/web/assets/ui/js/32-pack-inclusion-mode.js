// ── Context Packs: per-item inclusion mode + dry-run preview (v0.78) ────────────
//
// Additive companion to 16-context-packs.js's pack editor panel (#pack-path-editor). Every
// piece of DOM this file needs is created here at runtime — index.html and 16-context-packs.js
// are never edited — and it hooks in by WRAPPING the existing global `refreshPackEditorPaths`/
// `closePackEditor` functions (captured by reference, then reassigned on `window`) rather than
// redeclaring them, since every `NN-name.js` fragment shares one global scope once concatenated
// and a same-named top-level `function` in two fragments would silently shadow the earlier one.
//
// Backend: `GET /api/packs/:name/items` (path + inclusion_mode list), `POST
// /api/packs/:name/items/mode` (set one item's mode — "reference" or "pinned"), and
// `GET /api/packs/:name/export?...&dry_run=true` (token/byte estimate as JSON, writes nothing).

(function () {
  var _panelEl = null;
  var _listEl = null;
  var _resultEl = null;

  // Build the panel once (idempotent across repeated pack-editor opens — #pack-path-editor
  // itself is never removed from the DOM, just shown/hidden by 16-context-packs.js).
  function ensurePanel() {
    if (_panelEl) return _panelEl;
    var host = document.getElementById('pack-path-editor');
    if (!host) return null;

    var wrap = document.createElement('div');
    wrap.id = 'pack-inclusion-mode-panel';
    wrap.style.marginTop = '10px';
    wrap.style.paddingTop = '8px';
    wrap.style.borderTop = '1px solid var(--border)';

    var heading = document.createElement('div');
    heading.style.fontSize = '12px';
    heading.style.color = 'var(--muted)';
    heading.style.marginBottom = '4px';
    heading.textContent = 'Content mode (per item)';
    wrap.appendChild(heading);

    var hint = document.createElement('div');
    hint.style.fontSize = '10px';
    hint.style.color = 'var(--muted)';
    hint.style.marginBottom = '6px';
    hint.textContent = 'Reference: resolved fresh at export time. Pinned: freezes the item’s indexed content now.';
    wrap.appendChild(hint);

    var list = document.createElement('div');
    list.id = 'pack-inclusion-mode-list';
    list.style.display = 'flex';
    list.style.flexDirection = 'column';
    list.style.gap = '4px';
    list.style.marginBottom = '10px';
    wrap.appendChild(list);
    _listEl = list;

    var dryRunRow = document.createElement('div');
    dryRunRow.style.display = 'flex';
    dryRunRow.style.gap = '6px';
    dryRunRow.style.alignItems = 'center';

    var label = document.createElement('label');
    label.style.fontSize = '12px';
    label.style.color = 'var(--muted)';
    label.textContent = 'Preview:';
    dryRunRow.appendChild(label);

    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'btn-sm';
    btn.textContent = 'Dry run (token estimate)';
    btn.title = 'Estimate this export’s token/byte cost without writing anything';
    btn.addEventListener('click', runDryRun);
    dryRunRow.appendChild(btn);

    wrap.appendChild(dryRunRow);

    var result = document.createElement('div');
    result.id = 'pack-dry-run-result';
    result.style.fontSize = '12px';
    result.style.color = 'var(--muted)';
    result.style.marginTop = '4px';
    result.style.minHeight = '14px';
    wrap.appendChild(result);
    _resultEl = result;

    host.appendChild(wrap);
    _panelEl = wrap;
    return wrap;
  }

  // Security note: `item.path` is a pack member path with no charset validation server-side —
  // same stored-XSS class 16-context-packs.js already guards against. It only ever reaches the
  // DOM via `textContent`/`title` (never `innerHTML`) and the `fetch()` JSON body below, never
  // interpolated into an HTML/attribute string.
  function renderItemRow(item) {
    var row = document.createElement('div');
    row.style.display = 'flex';
    row.style.justifyContent = 'space-between';
    row.style.alignItems = 'center';
    row.style.fontSize = '11px';
    row.style.gap = '6px';

    var pathEl = document.createElement('span');
    pathEl.style.overflow = 'hidden';
    pathEl.style.textOverflow = 'ellipsis';
    pathEl.style.whiteSpace = 'nowrap';
    pathEl.style.flex = '1';
    pathEl.title = item.path;
    pathEl.textContent = item.path;
    row.appendChild(pathEl);

    var select = document.createElement('select');
    select.setAttribute('aria-label', 'Inclusion mode for ' + item.path);
    select.style.fontSize = '11px';
    select.style.background = 'var(--surface)';
    select.style.color = 'var(--text)';
    select.style.border = '1px solid var(--border)';
    select.style.borderRadius = '4px';
    select.style.padding = '1px 4px';
    select.style.flexShrink = '0';
    [
      ['reference', 'Reference (live)'],
      ['pinned', 'Pinned (frozen)'],
    ].forEach(function (pair) {
      var opt = document.createElement('option');
      opt.value = pair[0];
      opt.textContent = pair[1];
      if (pair[0] === item.inclusion_mode) opt.selected = true;
      select.appendChild(opt);
    });
    select.addEventListener('change', function () {
      setItemMode(item.path, select.value, select);
    });
    row.appendChild(select);

    return row;
  }

  function setItemMode(path, mode, selectEl) {
    var name = window._currentPackName;
    if (!name) return;
    if (selectEl) selectEl.disabled = true;
    fetch('/api/packs/' + encodeURIComponent(name) + '/items/mode', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path: path, mode: mode }),
    })
      .then(function (r) {
        if (!r.ok) {
          return r.json().then(function (d) { throw new Error(d.error || 'Could not set inclusion mode.'); });
        }
        return r.json();
      })
      .then(function () {
        if (typeof toast === 'function') {
          toast((mode === 'pinned' ? 'Pinned: ' : 'Set to reference: ') + path, 'info');
        }
      })
      .catch(function (e) {
        if (typeof toast === 'function') toast('Inclusion mode error: ' + e.message, 'error');
        // Best-effort resync so the dropdown doesn't keep showing a mode that didn't take.
        refreshInclusionModeList(name);
      })
      .finally(function () {
        if (selectEl) selectEl.disabled = false;
      });
  }

  function refreshInclusionModeList(name) {
    var panel = ensurePanel();
    if (!panel || !_listEl) return;
    fetch('/api/packs/' + encodeURIComponent(name) + '/items')
      .then(function (r) { return r.json(); })
      .then(function (d) {
        var items = d.items || [];
        _listEl.textContent = '';
        if (items.length === 0) {
          var empty = document.createElement('span');
          empty.style.color = 'var(--muted)';
          empty.textContent = 'No paths yet.';
          _listEl.appendChild(empty);
          return;
        }
        items.forEach(function (item) { _listEl.appendChild(renderItemRow(item)); });
      })
      .catch(function () {
        _listEl.textContent = '';
        var err = document.createElement('span');
        err.style.color = 'var(--muted)';
        err.textContent = 'Couldn’t load inclusion modes.';
        _listEl.appendChild(err);
      });
  }

  function runDryRun() {
    var name = window._currentPackName;
    if (!name || !_resultEl) return;
    var fmtEl = document.getElementById('pack-export-format');
    var format = fmtEl ? fmtEl.value : 'xml';
    _resultEl.textContent = 'Estimating…';
    fetch(
      '/api/packs/' + encodeURIComponent(name) + '/export?format=' + encodeURIComponent(format) + '&dry_run=true'
    )
      .then(function (r) {
        if (!r.ok) {
          return r.json().then(function (d) { throw new Error(d.error || 'Dry run failed.'); });
        }
        return r.json();
      })
      .then(function (d) {
        _resultEl.textContent =
          '~' + d.approx_tokens + ' tokens (' + d.bytes + ' bytes) across ' + d.items_exported +
          ' item(s), as ' + d.format + '. Nothing was written.';
      })
      .catch(function (e) {
        _resultEl.textContent = 'Error: ' + e.message;
      });
  }

  // Wrap (never redeclare) the existing globals so this panel stays in sync with whatever
  // already refreshes/closes the pack editor — pack opened, path added/removed, editor closed.
  var _origRefresh = window.refreshPackEditorPaths;
  if (typeof _origRefresh === 'function') {
    window.refreshPackEditorPaths = function (name) {
      _origRefresh(name);
      refreshInclusionModeList(name);
    };
  }

  var _origClose = window.closePackEditor;
  if (typeof _origClose === 'function') {
    window.closePackEditor = function () {
      _origClose();
      if (_resultEl) _resultEl.textContent = '';
    };
  }
})();
