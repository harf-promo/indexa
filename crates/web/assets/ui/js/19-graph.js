// ── Signature graph (v0.18) ───────────────────────────────────────────────────
// Force-directed visualization of the file-to-file call graph. Pure vanilla SVG
// (createElementNS), no libraries — matches 12-treemap.js. Math.random is fine in
// the browser.

var GRAPH_NS = 'http://www.w3.org/2000/svg';
var graphScopeLoaded = false;
var graphData = null;
var graphLayout = null; // [{id, x, y, r, node}]

// Interaction state for the navigable knowledge-graph view (v0.36). Click a node to
// lock a persistent focus highlight; "Expand neighbors" re-fetches only that node's
// N-hop neighborhood (server-side via ?focus=&depth=). renderGraph publishes its
// highlight closures here so 25-graph-explore.js can drive focus from outside the
// render closure.
var graphState = {
  focusId: null,   // currently locked (clicked) node id, or null
  lockedId: null,  // same as focusId while a render is live (closure-visible marker)
  depth: 0,        // depth of the current focused fetch (0 = whole-scope, not focused)
  setHighlight: null, // fn(id): persistently dim non-neighbors of id
  clearHighlight: null, // fn(): remove all highlight classes
};

// Current scope = the scope <select> value (the user's "home" scope, a root).
function currentGraphScope() {
  var sel = document.getElementById('graph-scope');
  return sel ? sel.value : '';
}

// Populate the scope <select> from /api/roots once, then load the graph.
function loadGraph(scope) {  // eslint-disable-line no-unused-vars
  var sel = document.getElementById('graph-scope');
  if (!graphScopeLoaded && sel) {
    graphScopeLoaded = true;
    fetch('/api/roots')
      .then(function (r) { return r.json(); })
      .then(function (roots) {
        sel.innerHTML = (roots || []).map(function (r) {
          return '<option value="' + escapeHtml(r.path) + '">' + escapeHtml(r.name || r.path) + '</option>';
        }).join('');
        fetchGraph(scope || (roots && roots[0] && roots[0].path) || '');
      })
      .catch(function () { fetchGraph(scope || ''); });
    return;
  }
  fetchGraph(scope || (sel ? sel.value : ''));
}

function fetchGraph(scope, focus, depth) {
  var svg = document.getElementById('graph-svg');
  var meta = document.getElementById('graph-meta');
  if (svg) clearSvg(svg);
  if (meta) meta.textContent = 'Loading…';
  // A plain (non-focused) scope load clears any locked focus + its breadcrumb;
  // a focused load (Expand neighbors) keeps the focus marker so it re-locks on render.
  graphState.depth = focus ? (depth || 1) : 0;
  if (!focus) {
    graphState.focusId = null;
    graphState.lockedId = null;
    var bar = document.getElementById('graph-focus-bar');
    if (bar) bar.hidden = true;
  }
  var url = '/api/graph?limit=300' + (scope ? '&scope=' + encodeURIComponent(scope) : '');
  if (focus) url += '&focus=' + encodeURIComponent(focus) + '&depth=' + (depth || 1);
  // Knowledge-graph overlays: request the enabled layers (default none ⇒ no param ⇒ call graph
  // only, byte-identical request).
  var layers = [];
  if (graphState.semanticLayer) layers.push('semantic');
  if (graphState.categoryLayer) layers.push('category');
  if (graphState.packLayer) layers.push('pack');
  if (layers.length) url += '&layers=' + layers.join(',');
  fetch(url)
    .then(function (r) { return r.json(); })
    .then(function (d) {
      graphData = d;
      renderGraph(d);
    })
    .catch(function (e) {
      if (meta) meta.textContent = 'Error: ' + e.message;
      if (svg) {
        clearSvg(svg);
        var errTxt = document.createElementNS(GRAPH_NS, 'text');
        errTxt.setAttribute('x', '50%');
        errTxt.setAttribute('y', '50%');
        errTxt.setAttribute('text-anchor', 'middle');
        errTxt.setAttribute('dominant-baseline', 'middle');
        errTxt.setAttribute('fill', 'var(--muted)');
        errTxt.setAttribute('font-size', '13');
        errTxt.textContent = 'Graph failed to load — is the server running?';
        svg.appendChild(errTxt);
      }
    });
}

function renderGraph(d) {
  var svg = document.getElementById('graph-svg');
  var meta = document.getElementById('graph-meta');
  if (!svg) return;
  clearSvg(svg);

  var nodes = d.nodes || [];
  var edges = d.edges || [];
  // Communities overlay (opt-in): tint by community + emphasize hubs. `communityTint` lives in
  // 29-graph-communities.js (same bundle scope); guard for load order.
  var communityCount = (d.communities || []).length;
  var communityHubs = {};
  var communityRank = {}; // community id → size rank (0 = largest); `communities` is size-sorted
  (d.communities || []).forEach(function (c, i) { communityHubs[c.hub_path] = true; communityRank[c.id] = i; });
  var tintFn = (graphState.communitiesLayer && typeof communityTint === 'function')
    ? communityTint : null;
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
    meta.textContent = nodes.length + ' files · ' + edges.length + ' edges · node size = centrality (how many files depend on it)'
      + (d.truncated ? ' · ⚠ truncated (showing the heaviest)' : '')
      + resolvedNote;
  }

  var rect = svg.getBoundingClientRect();
  var W = rect.width || 800, H = rect.height || 500;

  // Pan/zoom viewport: identity at first paint (viewBox == pixel size), then wheel-zoom
  // + drag-pan adjust it. Re-rendering resets the view to the full graph. Handlers are
  // wired once (idempotent).
  graphState.view = { x: 0, y: 0, w: W, h: H, baseW: W };
  svg.setAttribute('viewBox', '0 0 ' + W + ' ' + H);
  wireGraphZoomPan(svg);

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
    return { source: byId[e.from], target: byId[e.to], weight: e.weight, tier: e.tier || 'bare', bridge: !!e.bridge };
  }).filter(function (l) { return l.source && l.target; });

  // Adjacency for hover highlighting.
  var neighbors = {};
  layout.forEach(function (o) { neighbors[o.id] = {}; });
  links.forEach(function (l) {
    neighbors[l.source.id][l.target.id] = true;
    neighbors[l.target.id][l.source.id] = true;
  });

  // Nodes/edges are drawn at their seed (circle) positions; the force layout then runs
  // as an animation below, so the graph visibly blooms into shape.

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
    ln.setAttribute('class', 'graph-edge tier-' + l.tier + (l.bridge ? ' graph-edge-bridge' : ''));
    ln.setAttribute('stroke-width', Math.max(0.5, Math.min(4, l.weight * 0.6)));
    gEdges.appendChild(ln);
    return { el: ln, link: l };
  });

  var nodeEls = layout.map(function (o) {
    var isHub = tintFn && communityHubs[o.id];
    var g = document.createElementNS(GRAPH_NS, 'g');
    g.setAttribute('class', 'graph-node' + (isHub ? ' graph-hub' : ''));
    g.setAttribute('transform', 'translate(' + o.x + ',' + o.y + ')');
    var c = document.createElementNS(GRAPH_NS, 'circle');
    c.setAttribute('r', o.r);
    c.setAttribute('class', 'graph-node-circle');
    // Fade peripheral nodes; central hubs render solid (keeps a 0.45 floor so
    // even leaf nodes stay visible).
    c.setAttribute('fill-opacity', (0.45 + 0.55 * o.prNorm).toFixed(2));
    // Communities overlay: tint the circle by community (data-viz layer only; off ⇒ the CSS
    // --accent fill stands, byte-identical render).
    if (tintFn && o.node.community != null) {
      c.setAttribute('fill', tintFn(communityRank[o.node.community], communityCount));
    }
    g.appendChild(c);
    // Label only for higher-degree nodes (keeps it readable); always on hover, and always for hubs.
    var lbl = document.createElementNS(GRAPH_NS, 'text');
    lbl.setAttribute('class', 'graph-node-label');
    lbl.setAttribute('x', o.r + 3);
    lbl.setAttribute('y', 3);
    lbl.textContent = o.label;
    if (!isHub && (o.node.in_degree + o.node.out_degree) < 4) lbl.style.display = 'none';
    g.appendChild(lbl);
    gNodes.appendChild(g);

    // Keyboard a11y (WS6): each node is focusable and describes its relationships;
    // focus reuses the hover highlight so Tab/arrows surface the same neighbor view
    // and tooltip a mouse hover does. (SVG <g> takes tabindex in modern browsers.)
    g.setAttribute('tabindex', '0');
    g.setAttribute('role', 'button');
    g.setAttribute('aria-label',
      o.label + ' — calls ' + o.node.out_degree + ' file(s), called by ' + o.node.in_degree
      + '. Press Enter to focus and expand its connections.');
    g.addEventListener('mouseenter', function (ev) { onNodeHover(o, true, ev); });
    g.addEventListener('mouseleave', function () { onNodeHover(o, false); });
    g.addEventListener('focus', function () {
      // No pointer coords on focus — anchor the tooltip to the node's own rect.
      var r = g.getBoundingClientRect();
      onNodeHover(o, true, { clientX: r.left + r.width / 2, clientY: r.top });
    });
    g.addEventListener('blur', function () { onNodeHover(o, false); });
    // Click / Enter / Space locks a persistent focus on this node (v0.36).
    // focusNode + the focus-bar live in 25-graph-explore.js (concatenated after).
    g.addEventListener('click', function () {
      if (typeof focusNode === 'function') focusNode(o.id);
    });
    g.addEventListener('keydown', function (ev) {
      if (ev.key === 'Enter' || ev.key === ' ') {
        ev.preventDefault();
        if (typeof focusNode === 'function') focusNode(o.id);
      }
    });
    return { el: g, label: lbl, obj: o };
  });

  // Highlight a node's neighborhood by id — shared by transient hover and the
  // persistent click focus. byId/neighbors are closure-local to this render.
  function setHighlight(id) {
    if (!neighbors[id]) return;
    nodeEls.forEach(function (ne) {
      var related = ne.obj.id === id || neighbors[id][ne.obj.id];
      ne.el.classList.toggle('dim', !related);
      ne.el.classList.toggle('focus', ne.obj.id === id);
      if (related && ne.obj.id !== id) ne.label.style.display = '';
    });
    lineEls.forEach(function (le) {
      var on2 = le.link.source.id === id || le.link.target.id === id;
      le.el.classList.toggle('hl', on2);
      le.el.classList.toggle('dim', !on2);
    });
  }
  function clearHighlight() {
    nodeEls.forEach(function (ne) {
      ne.el.classList.remove('dim', 'focus');
      if ((ne.obj.node.in_degree + ne.obj.node.out_degree) < 4) ne.label.style.display = 'none';
    });
    lineEls.forEach(function (le) { le.el.classList.remove('hl', 'dim'); });
  }
  function onNodeHover(o, on, ev) {
    var tip = document.getElementById('graph-tooltip');
    if (on) {
      setHighlight(o.id);
      if (tip) {
        tip.hidden = false;
        tip.innerHTML = '<strong>' + escapeHtml(o.label) + '</strong><br>'
          + '<span class="graph-tip-path">' + escapeHtml(o.id) + '</span><br>'
          + 'calls ' + o.node.out_degree + ' file(s) · called by ' + o.node.in_degree
          + '<br>importance (links from ' + o.node.in_degree + ' file' + (o.node.in_degree === 1 ? '' : 's') + '): ' + Math.round(o.prNorm * 100) + '/100';
        if (ev) { tip.style.left = (ev.clientX + 12) + 'px'; tip.style.top = (ev.clientY + 12) + 'px'; }
      }
    } else {
      if (tip) tip.hidden = true;
      // Leaving a transient hover: restore the locked focus if one is set, else clear.
      if (graphState.lockedId && neighbors[graphState.lockedId]) setHighlight(graphState.lockedId);
      else clearHighlight();
    }
  }

  // Publish highlight controls so the focus-bar handlers (25-graph-explore.js) can
  // lock/clear a focus from outside this render closure.
  graphState.setHighlight = setHighlight;
  graphState.clearHighlight = clearHighlight;

  // Re-apply a locked focus after a re-render — an "Expand neighbors" fetch
  // re-centers on the focused node, which is still present in the neighborhood.
  if (graphState.focusId && byId[graphState.focusId]) {
    graphState.lockedId = graphState.focusId;
    setHighlight(graphState.focusId);
  }

  // Legend + conditional bare-name caveat (built from live data in 25-graph-explore.js).
  if (typeof renderGraphLegend === 'function') renderGraphLegend(d);

  // Animate the force layout so the graph blooms from its seed circle into its final
  // shape — the shareable moment. Respects prefers-reduced-motion (settles instantly).
  animateGraphLayout(layout, links, lineEls, nodeEls, W, H);
}

// One Fruchterman-Reingold cooling step (repulsion + edge attraction + capped move).
// `k` = ideal edge length, `temp` = max displacement this step. Mutates node x/y.
function forceTick(nodes, links, W, H, k, temp) {
  var n = nodes.length;
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
  for (var e = 0; e < links.length; e++) {
    var s = links[e].source, t = links[e].target;
    var dx2 = s.x - t.x, dy2 = s.y - t.y;
    var d2 = Math.sqrt(dx2 * dx2 + dy2 * dy2) || 0.01;
    var att = (d2 * d2) / k;
    var ox = (dx2 / d2) * att, oy = (dy2 / d2) * att;
    s.vx -= ox; s.vy -= oy;
    t.vx += ox; t.vy += oy;
  }
  for (var m = 0; m < n; m++) {
    var disp = Math.sqrt(nodes[m].vx * nodes[m].vx + nodes[m].vy * nodes[m].vy) || 0.01;
    nodes[m].x += (nodes[m].vx / disp) * Math.min(disp, temp);
    nodes[m].y += (nodes[m].vy / disp) * Math.min(disp, temp);
    nodes[m].x = Math.max(20, Math.min(W - 20, nodes[m].x));
    nodes[m].y = Math.max(20, Math.min(H - 20, nodes[m].y));
  }
}

// Push the current layout positions into the SVG DOM (node transforms + edge endpoints).
function syncGraphPositions(lineEls, nodeEls) {
  for (var i = 0; i < nodeEls.length; i++) {
    var o = nodeEls[i].obj;
    nodeEls[i].el.setAttribute('transform', 'translate(' + o.x + ',' + o.y + ')');
  }
  for (var j = 0; j < lineEls.length; j++) {
    var l = lineEls[j].link;
    lineEls[j].el.setAttribute('x1', l.source.x); lineEls[j].el.setAttribute('y1', l.source.y);
    lineEls[j].el.setAttribute('x2', l.target.x); lineEls[j].el.setAttribute('y2', l.target.y);
  }
}

// Run the cooling schedule. With motion allowed, spread the ticks across animation frames
// (a few per frame) so the graph blooms; otherwise settle synchronously. A single rAF handle
// lives on graphState so a re-render cancels the previous animation.
function animateGraphLayout(layout, links, lineEls, nodeEls, W, H) {
  if (graphState._raf) { cancelAnimationFrame(graphState._raf); graphState._raf = null; }
  var n = layout.length;
  if (n === 0) return;
  var k = Math.sqrt((W * H) / n) * 0.8;
  var temp = W * 0.10;
  var total = n > 120 ? 150 : 250;
  var done = 0;

  var reduce = window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (reduce) {
    while (done < total) { forceTick(layout, links, W, H, k, temp); temp *= 0.97; done++; }
    syncGraphPositions(lineEls, nodeEls);
    return;
  }
  var perFrame = 3; // physics ticks per frame — visible motion without a slow settle
  function frame() {
    for (var s = 0; s < perFrame && done < total; s++) {
      forceTick(layout, links, W, H, k, temp);
      temp *= 0.97;
      done++;
    }
    syncGraphPositions(lineEls, nodeEls);
    graphState._raf = done < total ? requestAnimationFrame(frame) : null;
  }
  graphState._raf = requestAnimationFrame(frame);
}

// Apply the current pan/zoom viewport to the SVG viewBox.
function applyGraphView(svg) {
  var v = graphState.view;
  if (v) svg.setAttribute('viewBox', v.x + ' ' + v.y + ' ' + v.w + ' ' + v.h);
}

// Wire wheel-zoom (around the cursor) + drag-pan (on the empty background, not nodes) once.
// Coordinates are converted client→user space via the live viewBox so zoom tracks the cursor.
function wireGraphZoomPan(svg) {
  if (svg.__zoomWired) return;
  svg.__zoomWired = true;

  svg.addEventListener('wheel', function (ev) {
    var v = graphState.view;
    if (!v) return;
    ev.preventDefault();
    var rect = svg.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    var ux = v.x + (ev.clientX - rect.left) / rect.width * v.w;
    var uy = v.y + (ev.clientY - rect.top) / rect.height * v.h;
    var factor = ev.deltaY < 0 ? 0.85 : 1.18; // wheel up = zoom in
    var nw = Math.max(v.baseW * 0.2, Math.min(v.baseW * 2, v.w * factor));
    var ratio = nw / v.w;
    var nh = v.h * ratio;
    v.x = ux - (ux - v.x) * ratio;
    v.y = uy - (uy - v.y) * ratio;
    v.w = nw; v.h = nh;
    applyGraphView(svg);
  }, { passive: false });

  var panning = false, lastX = 0, lastY = 0;
  svg.addEventListener('mousedown', function (ev) {
    if (ev.target !== svg) return; // only the background grabs; nodes keep click-to-focus
    panning = true; lastX = ev.clientX; lastY = ev.clientY;
    svg.classList.add('graph-panning');
  });
  window.addEventListener('mousemove', function (ev) {
    if (!panning) return;
    var v = graphState.view;
    if (!v) return;
    var rect = svg.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    v.x -= (ev.clientX - lastX) / rect.width * v.w;
    v.y -= (ev.clientY - lastY) / rect.height * v.h;
    lastX = ev.clientX; lastY = ev.clientY;
    applyGraphView(svg);
  });
  window.addEventListener('mouseup', function () {
    if (panning) { panning = false; svg.classList.remove('graph-panning'); }
  });
}

function clearSvg(svg) { while (svg.firstChild) svg.removeChild(svg.firstChild); }
