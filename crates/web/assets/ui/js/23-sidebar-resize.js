/* ── Drag-to-resize the sidebar (WS2) ──────────────────────────────────────────
   The folder tree was a fixed 260px, so long folder names were clipped. This lets you
   drag the divider to widen it and see full names. Width is clamped (can't vanish or eat
   the workspace) and persisted to localStorage. Desktop only — on ≤1024px the sidebar is a
   slide-out drawer and the handle is display:none (13-responsive.css). */
(function () {
  var KEY = 'indexa.sidebarWidth';
  var MIN = 180, MAX = 620;
  var root = document.documentElement;

  // Restore a previously-dragged width.
  try {
    var saved = parseInt(localStorage.getItem(KEY), 10);
    if (saved >= MIN && saved <= MAX) root.style.setProperty('--sidebar-width', saved + 'px');
  } catch (_) { /* localStorage may be unavailable */ }

  var handle = document.getElementById('sidebar-resizer');
  if (!handle) return;
  var dragging = false;

  handle.addEventListener('mousedown', function (e) {
    e.preventDefault();
    dragging = true;
    handle.classList.add('dragging');
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
  });

  document.addEventListener('mousemove', function (e) {
    if (!dragging) return;
    // The sidebar's left edge is at viewport x=0, so its width is the pointer's x.
    var w = Math.max(MIN, Math.min(MAX, e.clientX));
    root.style.setProperty('--sidebar-width', w + 'px');
  });

  document.addEventListener('mouseup', function () {
    if (!dragging) return;
    dragging = false;
    handle.classList.remove('dragging');
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
    try {
      var cur = parseInt(getComputedStyle(root).getPropertyValue('--sidebar-width'), 10);
      if (cur >= MIN && cur <= MAX) localStorage.setItem(KEY, String(cur));
    } catch (_) { /* ignore */ }
  });
}());
