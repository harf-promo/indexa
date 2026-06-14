// ── Knowledge-graph navigation: legend, help, focus/expand (v0.36) ────────────
// Layered on top of 19-graph.js (concatenated before this file). 19-graph.js owns
// the SVG render + force layout and publishes its highlight controls + `graphState`;
// this file adds the legend, the plain-language "What is this?" help, and the
// click-to-focus / expand-neighbors / reset interactions. Pure vanilla DOM; reuses
// `escG`, `graphState`, `fetchGraph`, `currentGraphScope` from 19-graph.js.

// Toggle the plain-language graph explainer (mirrors toggleMapHelp in 07-map.js).
function toggleGraphHelp() {  // eslint-disable-line no-unused-vars
  var el = document.getElementById('graph-help');
  var btn = document.getElementById('graph-help-btn');
  if (!el) return;
  var show = el.hidden;
  el.hidden = !show;
  if (btn) btn.setAttribute('aria-expanded', show ? 'true' : 'false');
}

// Build the legend from the live response. Swatches are decorative (aria-hidden);
// the text label is the accessible content, so relations are never color/dash-only.
// The bare-name caveat row appears only when approximate edges are actually present
// (or were dropped by strict mode) — single source of honesty for the graph.
function renderGraphLegend(d) {
  var el = document.getElementById('graph-legend');
  if (!el) return;
  var items = [
    '<span class="glegend-item"><span class="glegend-dot glegend-dot-lg" aria-hidden="true"></span>Central file (many depend on it)</span>',
    '<span class="glegend-item"><span class="glegend-dot glegend-dot-sm" aria-hidden="true"></span>Leaf file (few depend on it)</span>',
    '<span class="glegend-item"><span class="glegend-edge glegend-edge-import" aria-hidden="true"></span>Import — a clear file reference</span>',
    '<span class="glegend-item"><span class="glegend-edge glegend-edge-scope" aria-hidden="true"></span>Same folder / same file</span>',
    '<span class="glegend-item"><span class="glegend-edge glegend-edge-bare" aria-hidden="true"></span>Approximate — matched by name only</span>',
  ];
  el.innerHTML = items.join('');
  var bare = (d && d.bare_edges) || 0;
  var caveat = '';
  if (bare > 0) {
    caveat = 'Dotted links are matched by function name only and may connect the wrong files when a name is reused.';
  } else if (d && d.strict) {
    caveat = 'Approximate name-only links are hidden (strict mode).';
  }
  if (caveat) {
    var c = document.createElement('p');
    c.className = 'glegend-caveat';
    c.textContent = caveat;
    el.appendChild(c);
  }
}

// Lock a persistent focus on a node (called from the node click/Enter handler in
// 19-graph.js). Dims everything but the node + its direct neighbors, and shows the
// focus breadcrumb with Expand / Reset actions.
function focusNode(id) {
  graphState.focusId = id;
  graphState.lockedId = id;
  if (graphState.setHighlight) graphState.setHighlight(id);
  showGraphFocusBar(id);
}

function showGraphFocusBar(id) {
  var bar = document.getElementById('graph-focus-bar');
  var label = document.getElementById('graph-focus-label');
  if (!bar) return;
  var base = String(id).split('/').pop() || id;
  if (label) {
    label.innerHTML = 'Focused: <strong>' + escG(base) + '</strong> '
      + '<span class="graph-focus-path">' + escG(id) + '</span>';
  }
  bar.hidden = false;
}

// Expand the focused node's neighborhood: re-fetch only its N-hop neighbors
// server-side (so a hub's real neighbors aren't lost to client truncation). First
// click → direct neighbors (depth 1); a second click widens to depth 2.
function expandFocusNeighbors() {  // eslint-disable-line no-unused-vars
  if (!graphState.focusId) return;
  var d = (graphState.depth && graphState.depth >= 1) ? 2 : 1;
  fetchGraph(currentGraphScope(), graphState.focusId, d);
}

// Clear the focus and return to the whole current scope.
function resetGraphView() {  // eslint-disable-line no-unused-vars
  graphState.focusId = null;
  graphState.lockedId = null;
  graphState.depth = 0;
  var bar = document.getElementById('graph-focus-bar');
  if (bar) bar.hidden = true;
  if (graphState.clearHighlight) graphState.clearHighlight();
  fetchGraph(currentGraphScope());
}
