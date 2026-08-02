// 30-graph-modules.js — the Map's "Modules" panel (4.6): reads the PERSISTED architecture map
// (`indexa graph --compute-modules`), distinct from the live "Communities" overlay
// (29-graph-communities.js tints the current view at request time from the call graph shown;
// this fetches a separate, request-independent table and renders it as its own card list, not a
// recoloring of the SVG). Shares `currentGraphScope`/`escapeHtml` with 19-graph.js/
// 08-util-palette-init.js.

// eslint-disable-next-line no-unused-vars
function toggleModulesPanel(on) {
  var panel = document.getElementById('graph-modules-panel');
  if (!panel) return;
  if (!on) {
    panel.hidden = true;
    panel.innerHTML = '';
    return;
  }
  panel.hidden = false;
  panel.innerHTML = '<h4>Architecture map</h4><p class="graph-modules-empty">Loading…</p>';
  var scope = currentGraphScope();
  fetch('/api/graph/modules?scope=' + encodeURIComponent(scope))
    .then(function (r) { return r.json(); })
    .then(renderModulesPanel)
    .catch(function () {
      panel.innerHTML = '<h4>Architecture map</h4><p class="graph-modules-empty">Failed to load.</p>';
    });
}

function renderModulesPanel(data) {
  var panel = document.getElementById('graph-modules-panel');
  if (!panel) return;
  var modules = (data && data.modules) || [];
  if (!modules.length) {
    panel.innerHTML = '<h4>Architecture map</h4>' +
      '<p class="graph-modules-empty">No modules computed yet — run <code>indexa graph --compute-modules</code>.</p>';
    return;
  }
  var html = '<h4>Architecture map (' + modules.length + ' module' + (modules.length === 1 ? '' : 's') + ')</h4>';
  html += modules.map(function (m) {
    var members = (m.members || []);
    var shown = members.slice(0, 8).map(function (p) {
      return '<li>' + escapeHtml(p) + '</li>';
    }).join('');
    var more = members.length > 8 ? '<li>… ' + (members.length - 8) + ' more</li>' : '';
    return '<div class="graph-module-card">' +
      '<span class="graph-module-label">' + escapeHtml(m.label) + '</span>' +
      '<span class="graph-module-meta">cohesion ' + Number(m.cohesion).toFixed(2) + ' · ' + members.length + ' file(s)</span>' +
      '<ul class="graph-module-members">' + shown + more + '</ul>' +
      '</div>';
  }).join('');
  panel.innerHTML = html;
}
