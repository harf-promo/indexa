// ── Signature graph (v0.18) ───────────────────────────────────────────────────
// Force-directed visualization of the file-to-file call graph. Pure vanilla SVG
// (createElementNS), no libraries — matches 12-treemap.js. Math.random is fine in
// the browser.

var GRAPH_NS = 'http://www.w3.org/2000/svg';
var graphScopeLoaded = false;
var graphData = null;
var graphLayout = null; // [{id, x, y, r, node}]

// Populate the scope <select> from /api/roots once, then load the graph.
function loadGraph(scope) {  // eslint-disable-line no-unused-vars
  var sel = document.getElementById('graph-scope');
  if (!graphScopeLoaded && sel) {
    graphScopeLoaded = true;
    fetch('/api/roots')
      .then(function (r) { return r.json(); })
      .then(function (roots) {
        sel.innerHTML = (roots || []).map(function (r) {
          return '<option value="' + escG(r.path) + '">' + escG(r.name || r.path) + '</option>';
        }).join('');
        fetchGraph(scope || (roots && roots[0] && roots[0].path) || '');
      })
      .catch(function () { fetchGraph(scope || ''); });
    return;
  }
  fetchGraph(scope || (sel ? sel.value : ''));
}

function fetchGraph(scope) {
  var svg = document.getElementById('graph-svg');
  var meta = document.getElementById('graph-meta');
  if (svg) clearSvg(svg);
  if (meta) meta.textContent = 'Loading…';
  var url = '/api/graph?limit=300' + (scope ? '&scope=' + encodeURIComponent(scope) : '');
  fetch(url)
    .then(function (r) { return r.json(); })
    .then(function (d) {
      graphData = d;
      renderGraph(d);
    })
    .catch(function (e) {
      if (meta) meta.textContent = 'Error: ' + e.message;
    });
}

function renderGraph(d) {
  var svg = document.getElementById('graph-svg');
  var meta = document.getElementById('graph-meta');
  if (!svg) return;
  clearSvg(svg);

  var nodes = d.nodes || [];
  var edges = d.edges || [];
  if (nodes.length === 0) {
    if (meta) meta.textContent = '';
    var t = document.createElementNS(GRAPH_NS, 'text');
    t.setAttribute('x', '50%'); t.setAttribute('y', '50%');
    t.setAttribute('text-anchor', 'middle'); t.setAttribute('dominant-baseline', 'middle');
    t.setAttribute('fill', 'var(--muted)'); t.setAttribute('font-size', '13');
    t.textContent = 'No call edges in this scope — run `indexa deep` on code first.';
    svg.appendChild(t);
    return;
  }

  if (meta) {
    // The bare-name caveat applies only to the bare remainder (v0.25 scoped
    // resolution). When bare edges remain, name them. When none remain, say so
    // honestly: in strict mode bare edges were *dropped*, not resolved, so we
    // must not claim "all scope-resolved".
    var bare = d.bare_edges || 0;
    var resolvedNote = bare > 0
      ? ' · ' + bare + ' bare-name (approximate)'
      : (d.strict ? ' · strict (bare-name dropped)' : ' · all scope-resolved');
    meta.textContent = nodes.length + ' files · ' + edges.length + ' edges · node size = centrality'
      + (d.truncated ? ' · ⚠ truncated (showing the heaviest)' : '')
      + resolvedNote;
  }

  var rect = svg.getBoundingClientRect();
  var W = rect.width || 800, H = rect.height || 500;

  // Centrality drives node size: normalize PageRank to the most-central node in
  // the displayed subgraph so the biggest hubs stand out regardless of scale.
  var maxPr = nodes.reduce(function (m, n) { return Math.max(m, n.pagerank || 0); }, 0) || 1;

  // Index nodes and seed positions on a circle (deterministic-ish start).
  var byId = {};
  var layout = nodes.map(function (n, i) {
    var prNorm = (n.pagerank || 0) / maxPr;        // 0..1, 1 = most central
    var angle = (i / nodes.length) * Math.PI * 2;
    var o = {
      id: n.path,
      label: n.label,
      node: n,
      prNorm: prNorm,
      x: W / 2 + Math.cos(angle) * (W * 0.3) + (Math.random() - 0.5) * 40,
      y: H / 2 + Math.sin(angle) * (H * 0.3) + (Math.random() - 0.5) * 40,
      vx: 0, vy: 0,
      r: Math.max(4, Math.min(22, 4 + Math.sqrt(prNorm) * 18)),
    };
    byId[n.path] = o;
    return o;
  });
  var links = edges.map(function (e) {
    return { source: byId[e.from], target: byId[e.to], weight: e.weight, tier: e.tier || 'bare' };
  }).filter(function (l) { return l.source && l.target; });

  // Adjacency for hover highlighting.
  var neighbors = {};
  layout.forEach(function (o) { neighbors[o.id] = {}; });
  links.forEach(function (l) {
    neighbors[l.source.id][l.target.id] = true;
    neighbors[l.target.id][l.source.id] = true;
  });

  runForceLayout(layout, links, W, H);

  // ── Draw ──
  var gEdges = document.createElementNS(GRAPH_NS, 'g');
  var gNodes = document.createElementNS(GRAPH_NS, 'g');
  svg.appendChild(gEdges);
  svg.appendChild(gNodes);

  var lineEls = links.map(function (l) {
    var ln = document.createElementNS(GRAPH_NS, 'line');
    ln.setAttribute('x1', l.source.x); ln.setAttribute('y1', l.source.y);
    ln.setAttribute('x2', l.target.x); ln.setAttribute('y2', l.target.y);
    // Tier styling: scoped edges (same-file/import/same-dir) are solid; bare
    // name-only matches render dashed + muted so "approximate" reads visually.
    ln.setAttribute('class', 'graph-edge tier-' + l.tier);
    ln.setAttribute('stroke-width', Math.max(0.5, Math.min(4, l.weight * 0.6)));
    gEdges.appendChild(ln);
    return { el: ln, link: l };
  });

  var nodeEls = layout.map(function (o) {
    var g = document.createElementNS(GRAPH_NS, 'g');
    g.setAttribute('class', 'graph-node');
    g.setAttribute('transform', 'translate(' + o.x + ',' + o.y + ')');
    var c = document.createElementNS(GRAPH_NS, 'circle');
    c.setAttribute('r', o.r);
    c.setAttribute('class', 'graph-node-circle');
    // Fade peripheral nodes; central hubs render solid (keeps a 0.45 floor so
    // even leaf nodes stay visible).
    c.setAttribute('fill-opacity', (0.45 + 0.55 * o.prNorm).toFixed(2));
    g.appendChild(c);
    // Label only for higher-degree nodes (keeps it readable); always on hover.
    var lbl = document.createElementNS(GRAPH_NS, 'text');
    lbl.setAttribute('class', 'graph-node-label');
    lbl.setAttribute('x', o.r + 3);
    lbl.setAttribute('y', 3);
    lbl.textContent = o.label;
    if ((o.node.in_degree + o.node.out_degree) < 4) lbl.style.display = 'none';
    g.appendChild(lbl);
    gNodes.appendChild(g);

    // Keyboard a11y (WS6): each node is focusable and describes its relationships;
    // focus reuses the hover highlight so Tab/arrows surface the same neighbor view
    // and tooltip a mouse hover does. (SVG <g> takes tabindex in modern browsers.)
    g.setAttribute('tabindex', '0');
    g.setAttribute('role', 'button');
    g.setAttribute('aria-label',
      o.label + ' — calls ' + o.node.out_degree + ' file(s), called by ' + o.node.in_degree);
    g.addEventListener('mouseenter', function (ev) { onNodeHover(o, true, ev); });
    g.addEventListener('mouseleave', function () { onNodeHover(o, false); });
    g.addEventListener('focus', function () {
      // No pointer coords on focus — anchor the tooltip to the node's own rect.
      var r = g.getBoundingClientRect();
      onNodeHover(o, true, { clientX: r.left + r.width / 2, clientY: r.top });
    });
    g.addEventListener('blur', function () { onNodeHover(o, false); });
    return { el: g, label: lbl, obj: o };
  });

  function onNodeHover(o, on, ev) {
    var tip = document.getElementById('graph-tooltip');
    if (on) {
      nodeEls.forEach(function (ne) {
        var related = ne.obj.id === o.id || neighbors[o.id][ne.obj.id];
        ne.el.classList.toggle('dim', !related);
        ne.el.classList.toggle('focus', ne.obj.id === o.id);
        if (related && ne.obj.id !== o.id) ne.label.style.display = '';
      });
      lineEls.forEach(function (le) {
        var on2 = le.link.source.id === o.id || le.link.target.id === o.id;
        le.el.classList.toggle('hl', on2);
        le.el.classList.toggle('dim', !on2);
      });
      if (tip) {
        tip.hidden = false;
        tip.innerHTML = '<strong>' + escG(o.label) + '</strong><br>'
          + '<span class="graph-tip-path">' + escG(o.id) + '</span><br>'
          + 'calls ' + o.node.out_degree + ' file(s) · called by ' + o.node.in_degree
          + '<br>centrality ' + Math.round(o.prNorm * 100) + ' / 100';
        if (ev) { tip.style.left = (ev.clientX + 12) + 'px'; tip.style.top = (ev.clientY + 12) + 'px'; }
      }
    } else {
      nodeEls.forEach(function (ne) {
        ne.el.classList.remove('dim', 'focus');
        if ((ne.obj.node.in_degree + ne.obj.node.out_degree) < 4) ne.label.style.display = 'none';
      });
      lineEls.forEach(function (le) { le.el.classList.remove('hl', 'dim'); });
      if (tip) tip.hidden = true;
    }
  }
}

// Compact Fruchterman-Reingold style force layout (~250 cooling ticks).
function runForceLayout(nodes, links, W, H) {
  var n = nodes.length;
  if (n === 0) return;
  var area = W * H;
  var k = Math.sqrt(area / n) * 0.8;     // ideal edge length
  var temp = W * 0.10;                    // initial max displacement
  var iters = n > 120 ? 150 : 250;

  for (var step = 0; step < iters; step++) {
    // Repulsion (O(n²) — bounded by the API's node cap).
    for (var i = 0; i < n; i++) {
      nodes[i].vx = 0; nodes[i].vy = 0;
      for (var j = 0; j < n; j++) {
        if (i === j) continue;
        var dx = nodes[i].x - nodes[j].x;
        var dy = nodes[i].y - nodes[j].y;
        var dist = Math.sqrt(dx * dx + dy * dy) || 0.01;
        var rep = (k * k) / dist;
        nodes[i].vx += (dx / dist) * rep;
        nodes[i].vy += (dy / dist) * rep;
      }
    }
    // Attraction along edges.
    for (var e = 0; e < links.length; e++) {
      var s = links[e].source, t = links[e].target;
      var dx2 = s.x - t.x, dy2 = s.y - t.y;
      var d2 = Math.sqrt(dx2 * dx2 + dy2 * dy2) || 0.01;
      var att = (d2 * d2) / k;
      var ox = (dx2 / d2) * att, oy = (dy2 / d2) * att;
      s.vx -= ox; s.vy -= oy;
      t.vx += ox; t.vy += oy;
    }
    // Apply with temperature cap + keep inside bounds.
    for (var m = 0; m < n; m++) {
      var disp = Math.sqrt(nodes[m].vx * nodes[m].vx + nodes[m].vy * nodes[m].vy) || 0.01;
      nodes[m].x += (nodes[m].vx / disp) * Math.min(disp, temp);
      nodes[m].y += (nodes[m].vy / disp) * Math.min(disp, temp);
      nodes[m].x = Math.max(20, Math.min(W - 20, nodes[m].x));
      nodes[m].y = Math.max(20, Math.min(H - 20, nodes[m].y));
    }
    temp *= 0.97; // cool down
  }
}

function clearSvg(svg) { while (svg.firstChild) svg.removeChild(svg.firstChild); }

function escG(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}
