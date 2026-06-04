/* ── Smart label (classification) in summary view ──
   Appended to the summary panel when /api/classifications?path= returns a row.
   Shows the auto-detected category with ✓ Confirm / ✗ Ignore actions.
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
    container.innerHTML = renderClassificationChip(rec, path);
  } catch(_) {}
}

function renderClassificationChip(rec, path) {
  var src = rec.source;
  var cat = escapeHtml(rec.category || 'unknown');
  var catLabel = cat.charAt(0).toUpperCase() + cat.slice(1);
  var html = '<div class="classify-chip-wrap">';

  if (src === 'user') {
    html += '<span class="classify-chip classify-confirmed" title="Confirmed by you">✓ ' + catLabel + '</span>' +
      '<button class="btn-sm classify-undo-btn" onclick="undoClassification(' + JSON.stringify(path) + ')">Undo</button>';
  } else if (src === 'ignored') {
    html += '<span class="classify-chip classify-ignored" title="Suggestion ignored">Ignored</span>' +
      '<button class="btn-sm classify-undo-btn" onclick="undoClassification(' + JSON.stringify(path) + ')">Undo</button>';
  } else {
    // auto — show confirm/ignore options with a category selector
    // Categories must match SemanticCategory enum in crates/core/src/smart_classify.rs
    var categories = ['code','media','archive','personal','work','system','other'];
    var opts = categories.map(function(c) {
      return '<option value="' + c + '"' + (c === rec.category ? ' selected' : '') + '>' +
        c.charAt(0).toUpperCase() + c.slice(1) + '</option>';
    }).join('');
    html += '<span class="classify-label">Smart label:</span>' +
      '<select class="classify-select" id="classify-cat-' + CSS.escape(path) + '" aria-label="Choose category">' + opts + '</select>' +
      '<button class="btn-sm classify-confirm-btn" onclick="confirmClassification(' + JSON.stringify(path) + ')">✓ Confirm</button>' +
      '<button class="btn-sm classify-ignore-btn" onclick="ignoreClassification(' + JSON.stringify(path) + ')">✗ Ignore</button>';
  }

  html += '</div>';
  return html;
}

async function confirmClassification(path) {
  var sel = document.querySelector('#classify-cat-' + CSS.escape(path));
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
