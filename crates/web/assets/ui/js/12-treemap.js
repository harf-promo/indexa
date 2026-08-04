/* ── Treemap view (coverage map) ── */
var treemapLoaded = false;
var treemapData = null;       // full root array from /api/map/treemap
var treemapStack = [];        // navigation stack: [{name, path, children}]
var treemapCurrentNode = null;
var treemapRootIndex = 0;     // which top-level root to show when multiple exist
var treemapSvgNS = 'http://www.w3.org/2000/svg';

// Coverage colours — keyed by coverage state from the backend
var TM_COV_COLORS = {
  'full':    '#22c55e',   // green  — all summaries built
  'partial': '#f97316',   // orange — some built / in progress
  'failed':  '#f43f5e',   // red    — summarization failed
  'none':    '#374151',   // grey   — no context yet
};
// Fallback if coverage field missing
var TM_COV_DEFAULT = '#374151';

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
    treemapCurrentNode = treemapData[treemapRootIndex] || treemapData[0];
    renderTreemapCurrent();

  } catch (e) {
    treemapLoaded = false; // allow retry on next tab visit
    if (svg) svg.innerHTML = '<text x="50%" y="50%" text-anchor="middle" dominant-baseline="middle" fill="var(--red)" font-size="13">Error: ' + escapeHtml(e.message) + ' — switch away and back to retry</text>';
  }
}

/* ── Root picker ── Renders a small pill row above the treemap when there are multiple roots.
   Prevents a large root (e.g. '/') from swallowing a small one ('projects') into one blue block. */
function renderRootPicker() {
  if (!treemapData || treemapData.length <= 1) return;
  var bc = document.getElementById('treemap-breadcrumb');
  if (!bc) return;
  // Remove a stale picker first — this runs on every (re-)render, so without this each refresh
  // stacked another pill row above the treemap.
  var stale = document.querySelector('.treemap-root-picker');
  if (stale) stale.remove();
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
      treemapCurrentNode = treemapData[i];
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
  var totalSize = children.reduce(function(s, c) { return s + c.size; }, 0) || 1;
  var totalArea = W * H;
  children.forEach(function(c) {
    // Give zero-chunk dirs a minimal area so they're still visible
    c._area = (Math.max(c.size, 1) / Math.max(totalSize, 1)) * totalArea;
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
// Default Map sub-view: the interactive knowledge graph is the flagship view, so opening
// Map lands on the force-directed graph (it blooms on entry), not the treemap table.
var mapSubView = 'graph';

function switchMapView(view) {
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
