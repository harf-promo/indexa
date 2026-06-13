/* ── Responsive sidebar drawer (WS4) ───────────────────────────────────────────
   On ≤1024px the sidebar (#tree-pane) is a fixed slide-out drawer over the
   workspace; the topbar hamburger (#sidebar-toggle) opens it, and the scrim or Esc
   closes it. On desktop the sidebar is a normal grid column and these are no-ops —
   the toggle is display:none and matchMedia is false. The 1024px literal mirrors
   --bp-tablet (01-tokens.css) and the @media rule in 13-responsive.css.
   Globals (toggleSidebar/openSidebar/closeSidebar) are referenced by inline
   onclick handlers in index.html. eslint-disable-line no-unused-vars */
(function () {
  var MOBILE = window.matchMedia('(max-width: 1024px)');

  function body() { return document.querySelector('.app-body'); }
  function isOpen() { var b = body(); return !!(b && b.classList.contains('sidebar-open')); }

  window.openSidebar = function () {
    var b = body();
    if (!b || !MOBILE.matches) return;
    b.classList.add('sidebar-open');
    var tgl = document.getElementById('sidebar-toggle');
    if (tgl) tgl.setAttribute('aria-expanded', 'true');
    // Move focus into the drawer for keyboard users (the search input is the
    // natural first stop).
    var first = document.getElementById('search-input');
    if (first) first.focus();
  };

  window.closeSidebar = function () {
    var b = body();
    if (!b || !b.classList.contains('sidebar-open')) return;
    b.classList.remove('sidebar-open');
    var tgl = document.getElementById('sidebar-toggle');
    if (tgl) tgl.setAttribute('aria-expanded', 'false');
    // Restore focus to the opener only if focus is still inside the closing drawer
    // (don't yank it away from wherever the user has since clicked).
    var pane = document.getElementById('tree-pane');
    if (pane && pane.contains(document.activeElement) && tgl) tgl.focus();
  };

  window.toggleSidebar = function () {
    if (isOpen()) { window.closeSidebar(); } else { window.openSidebar(); }
  };

  // Esc closes the drawer when it's open (small-viewport only — isOpen guards it).
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && isOpen()) { e.stopPropagation(); window.closeSidebar(); }
  });

  // Leaving the small-viewport range resets the drawer so the desktop grid is
  // never stuck behind a stale `sidebar-open` class.
  var onChange = function (e) { if (!e.matches) window.closeSidebar(); };
  if (MOBILE.addEventListener) { MOBILE.addEventListener('change', onChange); }
  else if (MOBILE.addListener) { MOBILE.addListener(onChange); } // older Safari
}());
