// ── Importance Weights (v0.8) ─────────────────────────────────────────────────

document.addEventListener('DOMContentLoaded', function () {
  loadWeights();
});

function loadWeights() {  // eslint-disable-line no-unused-vars
  fetch('/api/weights')
    .then(function (r) { return r.json(); })
    .then(renderWeightsList)
    .catch(function () {
      var el = document.getElementById('weights-list');
      if (el) el.innerHTML = '<span style="color:var(--muted);font-size:12px">Couldn\'t load — is the server running?</span>';
    });
}

function renderWeightsList(weights) {
  var el = document.getElementById('weights-list');
  if (!el) return;
  if (!weights || weights.length === 0) {
    el.innerHTML = '<span style="color:var(--muted)">No weights set yet.</span>';
    return;
  }
  el.innerHTML = '<table style="width:100%;border-collapse:collapse">'
    + '<tr style="color:var(--muted);font-size:11px"><th style="text-align:left;padding:2px 6px">Kind</th><th style="text-align:right;padding:2px 6px">Weight</th><th style="text-align:left;padding:2px 6px">Target</th><th></th></tr>'
    + weights.map(function (w) {
      var color = w.weight > 1 ? 'var(--green)' : w.weight < 1 ? 'var(--red)' : 'var(--muted)';
      return '<tr style="border-top:1px solid var(--border)">'
        + '<td style="padding:3px 6px;color:var(--muted)">' + escapeHtml(w.target_kind) + '</td>'
        + '<td style="padding:3px 6px;text-align:right;color:' + color + '">' + w.weight.toFixed(2) + '</td>'
        + '<td style="padding:3px 6px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:250px" title="' + escapeHtml(w.target) + '">' + escapeHtml(w.target) + '</td>'
        + '<td style="padding:3px 6px"><button class="btn-sm btn-danger" style="font-size:10px;padding:1px 6px" title="Delete weight" aria-label="Delete weight" onclick="deleteWeight(' + JSON.stringify(w.target_kind) + ',' + JSON.stringify(w.target) + ')">✕</button></td>'
        + '</tr>';
    }).join('')
    + '</table>';
}

function setWeight() {  // eslint-disable-line no-unused-vars
  var kindEl   = document.getElementById('weight-kind');
  var targetEl = document.getElementById('weight-target');
  var valueEl  = document.getElementById('weight-value');
  var statusEl = document.getElementById('weight-status');
  var btn = document.querySelector('button[onclick="setWeight()"]');
  var kind   = kindEl   ? kindEl.value   : 'file';
  var target = targetEl ? targetEl.value.trim() : '';
  var weight = valueEl  ? parseFloat(valueEl.value) : 1.0;
  if (!target) { if (statusEl) statusEl.textContent = 'Enter a path or category.'; return; }
  if (isNaN(weight) || weight < 0) { if (statusEl) statusEl.textContent = 'Weight must be ≥ 0.'; return; }
  if (btn) btn.disabled = true;
  fetch('/api/weights', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ target_kind: kind, target: target, weight: weight }),
  })
    .then(function (r) { return r.json().then(function (d) { return { ok: r.ok, d: d }; }); })
    .then(function (res) {
      if (!res.ok) { if (statusEl) statusEl.textContent = res.d.error || 'Error.'; return; }
      if (statusEl) statusEl.textContent = 'Weight saved.';
      if (targetEl) targetEl.value = '';
      loadWeights();
    })
    .catch(function (e) { if (statusEl) statusEl.textContent = 'Error: ' + e.message; })
    .finally(function () { if (btn) btn.disabled = false; });
}

function deleteWeight(kind, target) {  // eslint-disable-line no-unused-vars
  fetch('/api/weights', {
    method: 'DELETE',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ target_kind: kind, target: target }),
  }).then(function () { loadWeights(); });
}

function suggestWeights() {  // eslint-disable-line no-unused-vars
  var statusEl = document.getElementById('weight-status');
  if (statusEl) statusEl.textContent = 'Loading suggestions…';
  fetch('/api/weights/suggest?days=30')
    .then(function (r) { return r.json(); })
    .then(function (d) {
      var suggestions = d.suggestions || [];
      if (suggestions.length === 0) {
        if (statusEl) statusEl.textContent = 'No recent files found for suggestions.';
        return;
      }
      var el = document.getElementById('weights-list');
      if (!el) return;
      if (statusEl) statusEl.textContent = suggestions.length + ' suggestion(s) — click Apply to set them.';
      el.innerHTML = '<div style="margin-bottom:6px"><button class="btn-sm" onclick="applyWeightSuggestions(' + JSON.stringify(suggestions) + ')">Apply all suggestions</button></div>'
        + '<table style="width:100%;border-collapse:collapse">'
        + suggestions.map(function (s) {
          return '<tr style="border-top:1px solid var(--border)">'
            + '<td style="padding:3px 6px;text-align:right;color:var(--accent)">' + s.weight.toFixed(2) + '</td>'
            + '<td style="padding:3px 6px;font-size:11px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:300px">' + escapeHtml(s.path) + '</td>'
            + '</tr>';
        }).join('') + '</table>';
    })
    .catch(function (e) { if (statusEl) statusEl.textContent = 'Error: ' + e.message; });
}

function applyWeightSuggestions(suggestions) {  // eslint-disable-line no-unused-vars
  var pending = suggestions.length;
  suggestions.forEach(function (s) {
    var kind = s.path.endsWith('/') ? 'dir' : 'file';
    fetch('/api/weights', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ target_kind: kind, target: s.path, weight: s.weight, reason: 'recency' }),
    }).then(function () {
      pending--;
      if (pending === 0) { loadWeights(); }
    });
  });
}

