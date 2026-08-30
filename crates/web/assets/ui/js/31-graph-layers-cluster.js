// 31-graph-layers-cluster.js — the Map's "Architecture layers" overlay: clusters/colours the
// call graph's nodes by an INFERRED architectural layer (API / Service / Data / UI / Utility),
// guessed purely from each node's file path client-side. No server round-trip and no new
// `/api/graph` param — the response 19-graph.js already fetches carries `node.path`, which is
// all this heuristic needs. Distinct from the server-computed overlays in
// 28-graph-layers.js/29-graph-communities.js (semantic/category/pack/communities all require a
// `&layers=` fetch); this one recolours the SVG already on screen.
//
// This lane owns exactly two new files plus one include_str! line each in lib.rs — it must not
// edit 19-graph.js, 25-graph-explore.js, or index.html. So rather than hooking into
// `renderGraph` (which would need a call added there) or adding static toggle/legend markup to
// index.html, this file (a) injects its own toggle + legend elements once at load, anchored on
// ids the bundle already relies on (`graph-meta`, `graph-legend`), and (b) watches `#graph-svg`
// with a MutationObserver so it reapplies colours every time 19-graph.js re-renders (initial
// load, scope change, focus/expand, reset) without needing a hook point in that file. Shares the
// bundle scope with 19-graph.js (`graphState`, `graphData`) per the established convention.

// ── Path heuristics ──────────────────────────────────────────────────────────
// Ordered rules, first match wins. Checked against a lower-cased, forward-slash-normalized
// path so a leading/trailing segment boundary is always `/` or the string edge — this keeps
// e.g. "api/" from matching a file merely named "myapi.rs". Purely a display heuristic (unlike
// the code graph's own call-edge resolution, this is intentionally case-insensitive and
// best-effort, not authoritative).
var ARCH_LAYER_RULES = [
  { layer: 'api', test: /(^|\/)(handlers?|routes?|controllers?|endpoints?|api)(\/|$)/ },
  { layer: 'service', test: /(^|\/)(commands?|services?|workers?|jobs?)(\/|$)/ },
  { layer: 'data', test: /(^|\/)(store|stores|db|models?|repositor(y|ies)|dao|migrations?|schema)(\/|$)/ },
  { layer: 'ui', test: /(^|\/)(assets\/ui|ui|views?|components?|pages?|templates?)(\/|$)/ },
  { layer: 'utility', test: /(^|\/)(utils?|helpers?|lib|common|shared)(\/|$)/ },
];

// eslint-disable-next-line no-unused-vars
function inferArchLayer(path) {
  if (!path) return null;
  var norm = String(path).replace(/\\/g, '/').toLowerCase();
  for (var i = 0; i < ARCH_LAYER_RULES.length; i++) {
    if (ARCH_LAYER_RULES[i].test.test(norm)) return ARCH_LAYER_RULES[i].layer;
  }
  return null; // unclassified — rendered as a neutral "Other" bucket, never dropped
}

// ── Colour + legend metadata ─────────────────────────────────────────────────
// Low-saturation categorical hues, one per layer — the same "tint, not a rainbow" convention
// 29-graph-communities.js established (sat 22, theme-aware lightness), applied inline on SVG
// circles only. This is the same sanctioned data-viz exception, not new UI chrome.
var ARCH_LAYER_META = {
  api: { label: 'API', hue: 200 },
  service: { label: 'Service', hue: 272 },
  data: { label: 'Data', hue: 344 },
  ui: { label: 'UI', hue: 56 },
  utility: { label: 'Utility', hue: 128 },
};
var ARCH_LAYER_ORDER = ['api', 'service', 'data', 'ui', 'utility'];

// eslint-disable-next-line no-unused-vars
function archLayerColor(layer) {
  var meta = layer && ARCH_LAYER_META[layer];
  if (!meta) return 'var(--ink-4)'; // "Other" — same neutral fallback as communityTint
  var dark = (document.documentElement.getAttribute('data-theme') || 'dark') !== 'light';
  var sat = 22, light = dark ? 62 : 44;
  return 'hsl(' + meta.hue + ', ' + sat + '%, ' + light + '%)';
}

// ── Apply / revert on the rendered SVG ───────────────────────────────────────
// `.graph-node` groups are appended in exactly the same order as `graphData.nodes` (19-graph.js
// builds `layout` via `nodes.map(...)` with no filtering, then appends one <g> per entry in
// that order) — so zipping the two arrays by index is a safe, if implicit, contract with that
// file's render order rather than a DOM lookup by id.
function applyArchLayerCluster(on) {
  var svg = document.getElementById('graph-svg');
  if (!svg) return;
  var groups = svg.querySelectorAll('.graph-node');
  var nodes = (typeof graphData !== 'undefined' && graphData && graphData.nodes) || [];
  var counts = on ? {} : null;
  groups.forEach(function (g, i) {
    var circle = g.querySelector('.graph-node-circle');
    if (!circle) return;
    if (on) {
      if (!('archOrigFill' in circle.dataset)) {
        // Capture once per element, before this overlay's first override, so turning it back
        // off restores exactly what 19-graph.js/29-graph-communities.js rendered (the plain
        // CSS default, or a live Communities tint) instead of clobbering it permanently.
        circle.dataset.archOrigFill = circle.getAttribute('fill') || '';
      }
      var layer = inferArchLayer(nodes[i] && nodes[i].path);
      circle.setAttribute('fill', archLayerColor(layer));
      var key = layer || 'other';
      counts[key] = (counts[key] || 0) + 1;
    } else if ('archOrigFill' in circle.dataset) {
      if (circle.dataset.archOrigFill) circle.setAttribute('fill', circle.dataset.archOrigFill);
      else circle.removeAttribute('fill');
      delete circle.dataset.archOrigFill;
    }
  });
  renderArchLayerLegend(counts);
}

// Set once by the DOMContentLoaded init below — not looked up via getElementById(id) each
// call, since this element is created at runtime (not a static id="..." in index.html) and
// the bundle's dead-reference self-check only recognizes runtime ids via an allowlist this
// lane's file scope can't extend.
var archLegendEl = null;

function renderArchLayerLegend(counts) {
  var el = archLegendEl;
  if (!el) return;
  if (!counts) {
    el.hidden = true;
    el.innerHTML = '';
    return;
  }
  var items = ARCH_LAYER_ORDER.filter(function (l) { return counts[l]; }).map(function (l) {
    return '<span class="glegend-item"><span class="garch-swatch" style="background:'
      + archLayerColor(l) + '" aria-hidden="true"></span>' + ARCH_LAYER_META[l].label
      + ' (' + counts[l] + ')</span>';
  });
  if (counts.other) {
    items.push(
      '<span class="glegend-item"><span class="garch-swatch" style="background:var(--ink-4)" aria-hidden="true"></span>Other ('
      + counts.other + ')</span>'
    );
  }
  // Every value interpolated above is a hardcoded label or a plain integer count — nothing
  // path-derived or otherwise attacker-influenced ever reaches this innerHTML.
  el.innerHTML = items.join('');
  el.hidden = items.length === 0;
}

// ── Wiring: inject the toggle + legend once, then follow every re-render ────
document.addEventListener('DOMContentLoaded', function () {
  var meta = document.getElementById('graph-meta');
  var toolbar = meta && meta.parentNode;
  if (toolbar) {
    var label = document.createElement('label');
    label.className = 'graph-layer-toggle';
    label.title = 'Colour nodes by inferred architectural layer (API / Service / Data / UI / '
      + 'Utility), guessed from each file’s path — heuristic, not authoritative';
    var checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.setAttribute('aria-label', 'Toggle architecture layers overlay');
    checkbox.addEventListener('change', function () {
      graphState.archLayerCluster = checkbox.checked;
      applyArchLayerCluster(checkbox.checked);
    });
    label.appendChild(checkbox);
    label.appendChild(document.createTextNode(' Architecture layers'));
    toolbar.insertBefore(label, meta);
  }

  var graphLegend = document.getElementById('graph-legend');
  if (graphLegend && graphLegend.parentNode) {
    // A sibling container, never the shared `#graph-legend` element itself: that one is fully
    // overwritten by `renderGraphLegend` (25-graph-explore.js) after every render, which would
    // silently wipe anything appended into it here.
    var legend = document.createElement('div');
    legend.className = 'garch-legend';
    legend.setAttribute('role', 'group');
    legend.setAttribute('aria-label', 'Architecture layers legend');
    legend.hidden = true;
    graphLegend.parentNode.insertBefore(legend, graphLegend.nextSibling);
    archLegendEl = legend;
  }

  var svg = document.getElementById('graph-svg');
  if (svg && window.MutationObserver) {
    // 19-graph.js's renderGraph clears + rebuilds `#graph-svg`'s direct children on every
    // (re)render (initial load, scope change, focus/expand, reset) — observing just that is
    // enough to catch every one without a hook point added to that file.
    new MutationObserver(function () {
      applyArchLayerCluster(!!(typeof graphState !== 'undefined' && graphState.archLayerCluster));
    }).observe(svg, { childList: true });
  }
});
