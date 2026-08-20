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
    container.innerHTML = renderClassificationChip(rec, path);
    wireClassificationChip(container, path);
  } catch(_) {}
}

// Security note (stored-XSS fix): `path` is a filename/dirname straight from the
// filesystem — angle brackets and quotes are all legal there — so it must never be
// interpolated into an HTML attribute (the previous code built each button's click
// handler by concatenating JSON.stringify(path) straight into the attribute string,
// which let a hostile filename break out of the attribute and inject live markup).
// The buttons below carry no per-row identity in their markup at all; wireClassificationChip
// attaches the real handlers afterward via addEventListener with `path` captured in a
// closure, so the untrusted value never touches HTML.
function renderClassificationChip(rec, path) {
  var src = rec.source;
  var cat = escapeHtml(rec.category || 'unknown');
  var catLabel = cat.charAt(0).toUpperCase() + cat.slice(1);
  var html = '<div class="classify-chip-wrap">';

  if (src === 'user') {
    html += '<span class="classify-chip classify-confirmed" title="Confirmed by you">✓ ' + catLabel + '</span>' +
      '<button class="btn-sm classify-undo-btn" type="button">Undo</button>';
  } else if (src === 'ignored') {
    html += '<span class="classify-chip classify-ignored" title="Suggestion ignored">Ignored</span>' +
      '<button class="btn-sm classify-undo-btn" type="button">Undo</button>';
  } else {
    // auto — show confirm/ignore options with a category selector
    // Categories must match SemanticCategory enum in crates/core/src/smart_classify.rs
    var categories = ['code','media','archive','personal','work','system','other'];
    var opts = categories.map(function(c) {
      return '<option value="' + c + '"' + (c === rec.category ? ' selected' : '') + '>' +
        c.charAt(0).toUpperCase() + c.slice(1) + '</option>';
    }).join('');
    // No per-row id here on purpose: an earlier version built one from the untrusted
    // path via CSS.escape(path), on the theory that CSS.escape's output was
    // attribute-safe. It isn't — CSS.escape backslash-prefixes special characters
    // (e.g. `"` becomes the two characters `\"`), it does not hex-escape them, and
    // HTML attribute parsing does not honor backslash escapes at all: the real `"`
    // character CSS.escape still emits closes a double-quoted attribute early just
    // the same. Only one chip is ever rendered into #summary-classify at a time, so a
    // per-row id was never necessary — confirmClassification below finds this select
    // by class, scoped to the container, and the untrusted path never touches HTML.
    html += '<span class="classify-label">Smart label:</span>' +
      '<select class="classify-select" aria-label="Choose category">' + opts + '</select>' +
      '<button class="btn-sm classify-confirm-btn" type="button">✓ Confirm</button>' +
      '<button class="btn-sm classify-ignore-btn" type="button">✕ Ignore</button>';
  }

  html += '</div>';
  return html;
}

/* Attach the real click handlers after renderClassificationChip's HTML is in the DOM —
   see the security note above renderClassificationChip. */
function wireClassificationChip(container, path) {
  var undoBtn = container.querySelector('.classify-undo-btn');
  if (undoBtn) undoBtn.addEventListener('click', function () { undoClassification(path); });
  var confirmBtn = container.querySelector('.classify-confirm-btn');
  if (confirmBtn) confirmBtn.addEventListener('click', function () { confirmClassification(path); });
  var ignoreBtn = container.querySelector('.classify-ignore-btn');
  if (ignoreBtn) ignoreBtn.addEventListener('click', function () { ignoreClassification(path); });
}

async function confirmClassification(path) {
  var sel = document.querySelector('#summary-classify .classify-select');
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
