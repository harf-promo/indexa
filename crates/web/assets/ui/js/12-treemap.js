/* ── Treemap view (coverage map) ── */
var treemapLoaded = false;
var treemapData = null;       // full root array from /api/map/treemap
var treemapStack = [];        // navigation stack: [{name, path, children}]
var treemapCurrentNode = null;
var treemapRootIndex = 0;     // which top-level root to show when multiple exist
var treemapSvgNS = 'http://www.w3.org/2000/svg';

// Coverage colours — keyed by coverage state from the backend. Harf design-system tokens
// (css/01-tokens.css), not literal hex: these are `light-dark()`-aware and theme-flip
// correctly, unlike the hardcoded Tailwind hex this used to carry (which stayed a dark
// slate block on a white page in light mode) — same `setAttribute('fill', 'var(--…)')`
// pattern 19-graph.js already uses. --positive is teal (the Harf "active state" colour,
// not brand green, which is punctuation-only per the design system).
var TM_COV_COLORS = {
  'full':    'var(--positive)',  // teal   — all summaries built
  'partial': 'var(--warning)',   // amber  — some built / in progress
  'failed':  'var(--critical)',  // red    — summarization failed
  'none':    'var(--rule-2)',    // grey   — no context yet
};
// Fallback if coverage field missing
var TM_COV_DEFAULT = 'var(--rule-2)';

function covColor(node) {
  return TM_COV_COLORS[node.coverage] || TM_COV_DEFAULT;
}

function fmtChunks(n) {
  if (!n) return '0 chunks';
  if (n >= 1000) return (n / 1000).toFixed(1) + 'k chunks';
  return n + ' chunks';
}

// Keep fmtSize for backward compat (used in tooltip)
function fmtSize(bytes) {
  if (!bytes) return '0 B';
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1048576) return (bytes / 1024).toFixed(0) + ' KB';
  if (bytes < 1073741824) return (bytes / 1048576).toFixed(1) + ' MB';
  return (bytes / 1073741824).toFixed(2) + ' GB';
}

async function loadTreemap() {
  if (treemapLoaded) {
    renderTreemapCurrent();
    return;
  }
  treemapLoaded = true;

  var svg = document.getElementById('treemap-svg');
  if (svg) {
    svg.innerHTML = '<text x="50%" y="50%" text-anchor="middle" dominant-baseline="middle" fill="var(--muted)" font-size="13">Loading…</text>';
  }

  try {
    var r = await fetch('/api/map/treemap');
    if (!r.ok) throw new Error('HTTP ' + r.status);
    treemapData = await r.json();

    if (!treemapData || !treemapData.length) {
      if (svg) svg.innerHTML = '<text x="50%" y="50%" text-anchor="middle" dominant-baseline="middle" fill="var(--muted)" font-size="13">No summaries built yet — index a folder for search to populate the map (scanning only lists files).</text>';
      return;
    }

    renderRootPicker();
    treemapStack = [];
    treemapCurrentNode = applySingleChildDescent(treemapData[treemapRootIndex] || treemapData[0]);
    renderTreemapCurrent();

  } catch (e) {
    treemapLoaded = false; // allow retry on next tab visit
    if (svg) svg.innerHTML = '<text x="50%" y="50%" text-anchor="middle" dominant-baseline="middle" fill="var(--red)" font-size="13">Error: ' + escapeHtml(e.message) + ' — switch away and back to retry</text>';
  }
}

// Defense-in-depth companion to the server-side single-child descent
// (`build_coverage_treemap` in stats.rs, B1): the server already collapses a boring
// single-child directory chain before emitting a root, so this normally does nothing.
// If a landing node still has exactly one child whose own children were serialized,
// keep descending client-side rather than showing one boring cell — but stop the
// instant we'd land on a node whose `children` were never serialized at all (outside
// the depth-4 window): descending there would render "No sub-directories", which is
// worse than showing the one cell we already have. Pushes each skipped node onto
// `treemapStack` so the breadcrumb keeps its "up" trail.
function applySingleChildDescent(node) {
  while (node && node.children && node.children.length === 1 &&
         node.children[0].children && node.children[0].children.length) {
    treemapStack.push(node);
    node = node.children[0];
  }
  return node;
}

/* ── Root picker ── Renders a small pill row above the treemap when there are multiple roots.
   Prevents a large root (e.g. '/') from swallowing a small one ('projects') into one blue block. */
function renderRootPicker() {
  // Remove any picker left over from a previous loadTreemap() (e.g. the map auto-refresh
  // after a job finishes resets treemapLoaded and re-runs this) — otherwise pickers stack
  // up, one per refresh, instead of the current root set replacing the last one.
  document.querySelectorAll('.treemap-root-picker').forEach(function(el) { el.remove(); });
  if (!treemapData || treemapData.length <= 1) return;
  var bc = document.getElementById('treemap-breadcrumb');
  if (!bc) return;
  var picker = document.createElement('div');
  picker.className = 'treemap-root-picker';
  treemapData.forEach(function(root, i) {
    var btn = document.createElement('button');
    btn.className = 'treemap-root-btn' + (i === treemapRootIndex ? ' active' : '');
    btn.textContent = root.name || root.path || 'root ' + i;
    btn.title = root.path || '';
    btn.addEventListener('click', function() {
      treemapRootIndex = i;
      treemapStack = [];
      treemapCurrentNode = applySingleChildDescent(treemapData[i]);
      document.querySelectorAll('.treemap-root-btn').forEach(function(b, j) {
        b.classList.toggle('active', j === i);
      });
      renderTreemapCurrent();
    });
    picker.appendChild(btn);
  });
  // Insert before the breadcrumb
  bc.parentNode.insertBefore(picker, bc);
}

function renderTreemapCurrent() {
  if (!treemapCurrentNode) return;
  var svg = document.getElementById('treemap-svg');
  if (!svg) return;

  var W = svg.clientWidth  || svg.parentElement.clientWidth  || 600;
  var H = svg.clientHeight || svg.parentElement.clientHeight || 360;
  svg.setAttribute('viewBox', '0 0 ' + W + ' ' + H);

  var children = (treemapCurrentNode.children || []).slice();

  if (!children.length) {
    svg.innerHTML = '<text x="' + (W/2) + '" y="' + (H/2) + '" text-anchor="middle" dominant-baseline="middle" fill="var(--muted)" font-size="13">No sub-directories</text>';
    renderBreadcrumb();
    return;
  }

  // Sort by size (chunk count) descending, then assign areas
  children.sort(function(a, b) { return b.size - a.size; });
  // Normalize by the SUM OF THE CLAMPED sizes, not the raw sum: with the raw sum, every
  // all-zero-chunk sibling (scan-only, before deep/summarize has run) computes
  // `max(0,1) / max(0,1) = 1` — i.e. EVERY child claims the full area, and squarify's
  // greedy row-fill only ever places the first one, leaving the rest with no `_rect` at
  // all (drawCell then skips them). Clamping the denominator the same way as the
  // numerator makes zero-chunk siblings share the area equally instead of each claiming
  // all of it; real (non-zero) sizes are unaffected since sum(max(size,1)) ≈ sum(size).
  var totalWeight = children.reduce(function(s, c) { return s + Math.max(c.size, 1); }, 0) || 1;
  var totalArea = W * H;
  children.forEach(function(c) {
    // Give zero-chunk dirs a minimal (equal-share) area so they're still visible.
    c._area = (Math.max(c.size, 1) / totalWeight) * totalArea;
    c._color = covColor(c);
    c._hasChildren = c.children && c.children.length > 0;
  });

  squarify(children, 0, 0, W, H);

  svg.innerHTML = '';
  var clipIdx = 0; // monotonic counter → unique clip-path IDs (no path-encoding collisions)
  children.forEach(function(node) {
    drawCell(svg, node, clipIdx++);
  });

  renderBreadcrumb();
}

// idx: monotonic per-render counter; drives unique clip-path IDs
function drawCell(svg, node, idx) {
  var r = node._rect;
  if (!r || r.w < 2 || r.h < 2) return;

  var g = document.createElementNS(treemapSvgNS, 'g');
  g.setAttribute('class', 'treemap-cell');
  g.setAttribute('data-path', node.path);
  g.setAttribute('data-name', node.name);
  g.setAttribute('data-size', node.size);
  g.setAttribute('data-files', node.file_count);

  // Rect
  var rect = document.createElementNS(treemapSvgNS, 'rect');
  rect.setAttribute('x', r.x + 1);
  rect.setAttribute('y', r.y + 1);
  rect.setAttribute('width', Math.max(0, r.w - 2));
  rect.setAttribute('height', Math.max(0, r.h - 2));
  rect.setAttribute('fill', node._color);
  rect.setAttribute('rx', '3');
  g.appendChild(rect);

  // Labels — only when cell is large enough
  if (r.w > 36 && r.h > 22) {
    var pad = 5;
    var clipId = 'tmc-' + idx; // index-based → guaranteed unique per render

    var txt = document.createElementNS(treemapSvgNS, 'text');
    txt.setAttribute('class', 'treemap-label');
    txt.setAttribute('x', r.x + pad);
    txt.setAttribute('y', r.y + pad);
    txt.setAttribute('clip-path', 'url(#' + clipId + ')');
    txt.textContent = node.name;
    g.appendChild(txt);

    if (r.h > 38) {
      var sub = document.createElementNS(treemapSvgNS, 'text');
      sub.setAttribute('class', 'treemap-label-sub');
      sub.setAttribute('x', r.x + pad);
      sub.setAttribute('y', r.y + pad + 16);
      sub.textContent = fmtChunks(node.size);
      g.appendChild(sub);
    }

    // Clip path so text doesn't overflow the cell
    var defs = svg.querySelector('defs') || (function() {
      var d = document.createElementNS(treemapSvgNS, 'defs');
      svg.insertBefore(d, svg.firstChild);
      return d;
    }());
    var cp = document.createElementNS(treemapSvgNS, 'clipPath');
    cp.setAttribute('id', clipId);
    var cpr = document.createElementNS(treemapSvgNS, 'rect');
    cpr.setAttribute('x', r.x + 1);
    cpr.setAttribute('y', r.y + 1);
    cpr.setAttribute('width', Math.max(0, r.w - 4));
    cpr.setAttribute('height', Math.max(0, r.h - 4));
    cp.appendChild(cpr);
    defs.appendChild(cp);
  }

  // Click + keyboard: drill down if the node has children
  if (node._hasChildren) {
    g.style.cursor = 'pointer';
    g.setAttribute('tabindex', '0');
    g.setAttribute('role', 'button');
    g.setAttribute('aria-label', node.name + ' — ' + fmtChunks(node.size) + ' — click to drill down');
    function drillIn() {
      treemapStack.push(treemapCurrentNode);
      treemapCurrentNode = node;
      renderTreemapCurrent();
    }
    g.addEventListener('click', drillIn);
    g.addEventListener('keydown', function(e) {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); drillIn(); }
    });
  } else {
    g.setAttribute('aria-label', node.name + ' — ' + fmtChunks(node.size));
  }

  // Hover tooltip
  g.addEventListener('mouseenter', function(e) { showTreemapTooltip(e, node); });
  g.addEventListener('mousemove',  function(e) { moveTreemapTooltip(e); });
  g.addEventListener('mouseleave', function()  { hideTreemapTooltip(); });

  svg.appendChild(g);
}


/* ── Tooltip ── */
function showTreemapTooltip(e, node) {
  var tip = document.getElementById('treemap-tooltip');
  if (!tip) return;
  var covLabel = { full: '● Built', partial: '◐ In progress', failed: '✕ Failed', none: '○ Not built' };
  tip.innerHTML =
    '<strong>' + escapeHtml(node.name) + '</strong>' +
    '<span style="color:var(--muted)">' + escapeHtml(node.path) + '</span><br>' +
    fmtChunks(node.size) +
    (node.coverage ? ' &middot; ' + escapeHtml(covLabel[node.coverage] || node.coverage) : '') +
    (node._hasChildren ? '<br><span style="color:var(--accent);font-size:11px">Click to drill down</span>' : '');
  tip.hidden = false;
  moveTreemapTooltip(e);
}

function moveTreemapTooltip(e) {
  var tip = document.getElementById('treemap-tooltip');
  if (!tip || tip.hidden) return;
  var x = e.clientX + 14, y = e.clientY + 14;
  var tw = tip.offsetWidth, th = tip.offsetHeight;
  if (x + tw > window.innerWidth - 8)  x = e.clientX - tw - 14;
  if (y + th > window.innerHeight - 8) y = e.clientY - th - 14;
  tip.style.left = x + 'px';
  tip.style.top  = y + 'px';
}

function hideTreemapTooltip() {
  var tip = document.getElementById('treemap-tooltip');
  if (tip) tip.hidden = true;
}

/* ── Breadcrumb ── */
function renderBreadcrumb() {
  var bc = document.getElementById('treemap-breadcrumb');
  if (!bc) return;
  bc.innerHTML = '';

  var allNodes = treemapStack.concat([treemapCurrentNode]);
  allNodes.forEach(function(node, i) {
    if (i > 0) {
      var sep = document.createElement('span');
      sep.className = 'treemap-crumb-sep';
      sep.textContent = '›';
      sep.setAttribute('aria-hidden', 'true'); // decorative separator — skip for AT
      bc.appendChild(sep);
    }
    var isCurrent = i === allNodes.length - 1;
    if (isCurrent) {
      var span = document.createElement('span');
      span.className = 'treemap-crumb current';
      span.textContent = node.name || node.path || 'All roots';
      span.setAttribute('aria-current', 'page'); // marks the active drill-down level
      bc.appendChild(span);
    } else {
      var btn = document.createElement('button');
      btn.className = 'treemap-crumb';
      btn.textContent = node.name || node.path || 'All roots';
      (function(idx) {
        btn.addEventListener('click', function() {
          treemapStack = treemapStack.slice(0, idx);
          treemapCurrentNode = allNodes[idx];
          renderTreemapCurrent();
        });
      }(i));
      bc.appendChild(btn);
    }
  });
}

/* ── Map sub-view toggle ── */
// Default is coverage Treemap (the useful picture at whole-disk scope). Graph
// is the right default once the user has scoped into a project-depth folder.
// An explicit tab click sticks for the rest of the session.
var mapSubView = 'treemap';
var mapUserPicked = false;

function pickMapView() {
  if (mapUserPicked && mapSubView) return mapSubView;
  if (!selectedPath) return 'treemap';
  var depth = String(selectedPath).split('/').filter(Boolean).length;
  // /Users/name/development/projects/indexa → 5 segments: a project, not the disk.
  return depth >= 5 ? 'graph' : 'treemap';
}

function switchMapView(view, fromUser) {
  if (fromUser !== false) mapUserPicked = true;
  mapSubView = view;
  ['treemap', 'table', 'graph'].forEach(function(v) {
    var btn   = document.getElementById('map-tab-' + v);
    var panel = document.getElementById('map-panel-' + v);
    var active = v === view;
    if (btn)   { btn.classList.toggle('active', active); btn.setAttribute('aria-selected', active ? 'true' : 'false'); }
    if (panel) panel.hidden = !active;
  });
  if (view === 'treemap') loadTreemap();
  if (view === 'table')   loadMap();
  if (view === 'graph' && typeof loadGraph === 'function') loadGraph();
}

/* ── Squarified treemap layout ─────────────────────────────────────────────── */
// Items must have ._area set before calling. After the call each item has ._rect = {x,y,w,h}.

function squarify(items, x0, y0, x1, y1) {
  if (!items.length) return;
  var total = items.reduce(function(s, c) { return s + c._area; }, 0);
  if (!total) return;

  var i = 0, n = items.length;
  while (i < n) {
    var dx = x1 - x0, dy = y1 - y0;
    if (dx <= 0 || dy <= 0) break;

    // Greedily grow the current row
    var rowItems = [], rowArea = 0;
    var bestWorst = Infinity;
    var j = i;

    while (j < n) {
      var candidate = items[j];
      rowItems.push(candidate);
      rowArea += candidate._area;
      var w = tmWorst(rowItems, rowArea, Math.min(dx, dy));
      if (w <= bestWorst) {
        bestWorst = w;
        j++;
      } else {
        rowItems.pop();
        rowArea -= candidate._area;
        break;
      }
    }
    if (!rowItems.length) { rowItems.push(items[i]); rowArea = items[i]._area; j = i + 1; }

    // Place the row
    if (dx >= dy) {
      var rowW = rowArea / dy;
      var curY = y0;
      rowItems.forEach(function(node) {
        var h = node._area / rowW;
        node._rect = { x: x0, y: curY, w: rowW, h: h };
        curY += h;
      });
      x0 += rowW;
    } else {
      var rowH = rowArea / dx;
      var curX = x0;
      rowItems.forEach(function(node) {
        var w = node._area / rowH;
        node._rect = { x: curX, y: y0, w: w, h: rowH };
        curX += w;
      });
      y0 += rowH;
    }
    i = j;
  }
}

function tmWorst(row, rowArea, side) {
  if (!rowArea || !side) return Infinity;
  var maxA = 0, minA = Infinity;
  row.forEach(function(c) {
    if (c._area > maxA) maxA = c._area;
    if (c._area < minA) minA = c._area;
  });
  var s2 = side * side, ra2 = rowArea * rowArea;
  return Math.max(s2 * maxA / ra2, ra2 / (s2 * minA));
}

/* Re-render when the treemap tab is resized */
window.addEventListener('resize', function() {
  if (mapSubView === 'treemap' && treemapCurrentNode) {
    renderTreemapCurrent();
  }
});
