/* ── Smart label (classification) in summary view ──
   Appended to the summary panel when /api/classifications?path= returns a row.
   Shows the auto-detected category with ✓ Confirm / ✕ Ignore actions.
   Called from showSummary() after rendering. */

async function loadClassificationForPath(path) {
  var container = document.getElementById('summary-classify');
  if (!container) return;
  container.innerHTML = '';
  try {
    var r = await fetch('/api/classifications?path=' + encodeURIComponent(path));
    if (!r.ok) return;
    var rows = await r.json();
    if (!rows || !rows.length) return;
    var rec = rows[0];
    container.innerHTML = renderClassificationChip(rec);
    // Wire the action buttons with `path` from THIS closure — no user-controlled path is ever
    // placed into an HTML attribute, so there's no injection surface (and the buttons actually
    // fire; the old `onclick="fn(" + JSON.stringify(path) + ")"` broke on the JSON quotes).
    container.querySelectorAll('button[data-act]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var act = btn.getAttribute('data-act');
        if (act === 'undo') undoClassification(path);
        else if (act === 'confirm') confirmClassification(path);
        else if (act === 'ignore') ignoreClassification(path);
      });
    });
  } catch(_) {}
}

function renderClassificationChip(rec) {
  var src = rec.source;
  var cat = escapeHtml(rec.category || 'unknown');
  var catLabel = cat.charAt(0).toUpperCase() + cat.slice(1);
  var html = '<div class="classify-chip-wrap">';

  if (src === 'user') {
    html += '<span class="classify-chip classify-confirmed" title="Confirmed by you">✓ ' + catLabel + '</span>' +
      '<button class="btn-sm classify-undo-btn" data-act="undo">Undo</button>';
  } else if (src === 'ignored') {
    html += '<span class="classify-chip classify-ignored" title="Suggestion ignored">Ignored</span>' +
      '<button class="btn-sm classify-undo-btn" data-act="undo">Undo</button>';
  } else {
    // auto — show confirm/ignore options with a category selector
    // Categories must match SemanticCategory enum in crates/core/src/smart_classify.rs
    var categories = ['code','media','archive','personal','work','system','other'];
    var opts = categories.map(function(c) {
      return '<option value="' + c + '"' + (c === rec.category ? ' selected' : '') + '>' +
        c.charAt(0).toUpperCase() + c.slice(1) + '</option>';
    }).join('');
    // Fixed id (only one chip is shown at a time, in #summary-classify). The old id used
    // CSS.escape(path), which put literal backslashes in the id but resolved them away in the
    // querySelector lookup, so the two never matched → Confirm always fell back to 'other'.
    html += '<span class="classify-label">Smart label:</span>' +
      '<select class="classify-select" id="classify-cat-select" aria-label="Choose category">' + opts + '</select>' +
      '<button class="btn-sm classify-confirm-btn" data-act="confirm">✓ Confirm</button>' +
      '<button class="btn-sm classify-ignore-btn" data-act="ignore">✕ Ignore</button>';
  }

  html += '</div>';
  return html;
}

async function confirmClassification(path) {
  var sel = document.getElementById('classify-cat-select');
  var category = sel ? sel.value : 'other';
  try {
    var r = await fetch('/api/classifications/confirm', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ path: path, category: category })
    });
    var d = await r.json();
    if (d.confirmed) { toast('Classification confirmed: ' + category, 'info'); loadClassificationForPath(path); }
    else toast(d.error || 'Failed', 'error');
  } catch(e) { toast('Error: ' + e.message, 'error'); }
}

async function ignoreClassification(path) {
  try {
    var r = await fetch('/api/classifications/ignore', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ path: path })
    });
    var d = await r.json();
    if (d.ignored) { toast('Suggestion ignored', 'info'); loadClassificationForPath(path); }
    else toast(d.error || 'Failed', 'error');
  } catch(e) { toast('Error: ' + e.message, 'error'); }
}

async function undoClassification(path) {
  // Delete the classification row entirely — reverts to "no suggestion".
  // Re-running `indexa classify` will re-surface the auto suggestion.
  try {
    var r = await fetch('/api/classifications/reset', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ path: path })
    });
    if (!r.ok) { toast('Reset failed (' + r.status + ')', 'error'); return; }
    toast('Label cleared — run `indexa classify` to regenerate a suggestion', 'info');
    loadClassificationForPath(path);
  } catch(e) { toast('Error: ' + e.message, 'error'); }
}
