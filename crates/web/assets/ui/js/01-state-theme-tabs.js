'use strict';

/* ── Harf icon marks ──
   Single-stroke inline SVG icons (currentColor, stroke-width 1.75), used where a glyph
   is genuinely needed instead of punctuation. Shared across the concatenated bundle. */
function uiIco(inner) {
  return '<svg class="ui-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
    'stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    inner + '</svg>';
}
const ICO_EYE = uiIco('<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7z"/><circle cx="12" cy="12" r="3"/>');
const ICO_EYE_OFF = uiIco('<path d="M2 12s3.5-7 10-7c2 0 3.7.6 5.1 1.5"/><path d="M21.5 14.5A18 18 0 0 1 12 19C5 19 2 12 2 12"/><line x1="3" y1="3" x2="21" y2="21"/>');
const ICO_BELL_ON = uiIco('<path d="M18 8a6 6 0 1 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.7 21a2 2 0 0 1-3.4 0"/>');
const ICO_BELL_OFF = uiIco('<path d="M18 8a6 6 0 1 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.7 21a2 2 0 0 1-3.4 0"/><line x1="3" y1="3" x2="21" y2="21"/>');
const ICO_SEARCH = uiIco('<circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>');
const ICO_GEAR = uiIco('<circle cx="12" cy="12" r="3"/><path d="M12 2v3M12 19v3M4.2 4.2l2.1 2.1M17.7 17.7l2.1 2.1M2 12h3M19 12h3M4.2 19.8l2.1-2.1M17.7 6.3l2.1-2.1"/>');
const ICO_REFRESH = uiIco('<path d="M21 12a9 9 0 1 1-2.64-6.36"/><polyline points="21 3 21 9 15 9"/>');
const ICO_CLIPBOARD = uiIco('<rect x="8" y="3" width="8" height="4" rx="1"/><path d="M16 5h2a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2"/>');
const ICO_PENCIL = uiIco('<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/>');
const ICO_BOLT = uiIco('<polygon points="13 2 4 14 11 14 10 22 20 9 13 9 13 2"/>');
const ICO_TRASH = uiIco('<polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><path d="M10 11v6M14 11v6"/><path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/>');
const ICO_FOLDER = uiIco('<path d="M3 7a2 2 0 0 1 2-2h4l2 2.5h8a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>');
const ICO_FILE = uiIco('<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><polyline points="14 3 14 8 19 8"/>');
const ICO_CHAT = uiIco('<path d="M21 11.5a8.4 8.4 0 0 1-9 8.4 9 9 0 0 1-3.9-.9L3 20l1-4.1A8.4 8.4 0 1 1 21 11.5z"/>');
const ICO_PLUS = uiIco('<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>');
const ICO_MAP = uiIco('<polygon points="3 6 9 3 15 6 21 3 21 18 15 21 9 18 3 21 3 6"/><line x1="9" y1="3" x2="9" y2="18"/><line x1="15" y1="6" x2="15" y2="21"/>');
const ICO_MOON = uiIco('<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>');
const ICO_DOWNLOAD = uiIco('<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/>');

/* ── State ── */
// currentTab tracks the logically-active surface for legacy callers (jobs rendering
// guards on currentTab === 'jobs'). currentView is the workspace view (tree|chat|map);
// Settings + Activity are drawers that open OVER the workspace without changing the view.
let currentTab = 'tree';
let currentView = 'tree';
let selectedPath = null;
const expandedPaths = new Set();
/* Per-path subtree context-coverage rollup ({covered, partial, total}), stashed as
   tree/search/child rows are built so the summary header can show a "context: N%" chip
   without an extra round-trip. Keyed by entry path; stale entries are harmless (lookups
   are by current path only). */
const coverageByPath = {};

/* ── Theme ──
   Harf marks: single-stroke inline SVG (currentColor). The button shows a moon while
   in dark mode (tap to go light) and a sun while in light mode (tap to go dark). */
const THEME_ICON_MOON = '<svg class="ui-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>';
const THEME_ICON_SUN = '<svg class="ui-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><line x1="12" y1="2" x2="12" y2="5"/><line x1="12" y1="19" x2="12" y2="22"/><line x1="2" y1="12" x2="5" y2="12"/><line x1="19" y1="12" x2="22" y2="12"/><line x1="4.9" y1="4.9" x2="6.9" y2="6.9"/><line x1="17.1" y1="17.1" x2="19.1" y2="19.1"/><line x1="4.9" y1="19.1" x2="6.9" y2="17.1"/><line x1="17.1" y1="6.9" x2="19.1" y2="4.9"/></svg>';
(function initTheme() {
  const saved = localStorage.getItem('indexa_theme') || 'dark';
  document.documentElement.setAttribute('data-theme', saved);
  const btn = document.getElementById('theme-toggle');
  if (btn) btn.innerHTML = saved === 'light' ? THEME_ICON_SUN : THEME_ICON_MOON;
})();

function toggleTheme() {
  const current = document.documentElement.getAttribute('data-theme') || 'dark';
  const next = current === 'dark' ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  localStorage.setItem('indexa_theme', next);
  const btn = document.getElementById('theme-toggle');
  if (btn) btn.innerHTML = next === 'light' ? THEME_ICON_SUN : THEME_ICON_MOON;
}

/* ── Navigation ──
   'tree' | 'chat' | 'map'            → workspace views (in-place toggle)
   'settings' | 'jobs' | 'review'     → drawers opened over the workspace
   Single entry point so every existing caller (showSummary→'tree', doAsk→'chat',
   fireJob→'jobs', the pill, the gear) keeps working. */
function switchTab(tab) {
  if (tab === 'settings' || tab === 'jobs' || tab === 'review') { openDrawer(tab); return; }

  currentView = tab;
  currentTab = tab;
  ['tree','chat','map'].forEach(function(t) {
    const btn = document.getElementById('view-' + t);
    if (btn) {
      btn.classList.toggle('active', t === tab);
      btn.setAttribute('aria-selected', t === tab ? 'true' : 'false');
    }
    const panel = document.getElementById('panel-' + t);
    if (panel) panel.classList.toggle('active', t === tab);
  });
  const sv = document.getElementById('summary-view');
  if (sv) sv.style.display = (tab === 'tree' && selectedPath !== null) ? 'block' : '';
  // Enter the Map tab on its ACTIVE sub-view (graph by default), not unconditionally the
  // table — otherwise the default graph panel shows empty until clicked. Mirrors refreshMap.
  if (tab === 'map') {
    if (typeof switchMapView === 'function') switchMapView(typeof mapSubView !== 'undefined' && mapSubView ? mapSubView : 'graph');
    else loadMap();
  }
  // Mirror the active tab into the URL (deep-linking, v0.37). Drawers returned above.
  if (typeof writeHash === 'function') writeHash();
}

/* The element focus returns to when the last open drawer closes (the gear/activity
   button that opened it). */
let lastDrawerOpener = null;

/* Every top-level region BEHIND the drawer overlays. Each is a sibling of (not inside) the
   drawers, so making them `inert` while a drawer is open removes them from the tab order,
   hit-testing, and the accessibility tree — leaving the open drawer the only interactive
   region (a complete focus trap). #toast is deliberately omitted: it's a non-focusable
   aria-live status region that should keep announcing while a drawer is open. Keep this
   list in sync with the top-level focusable siblings in index.html. */
const DRAWER_BACKGROUND_REGIONS = ['.app-topbar', '.app-body', '#engine-bar', '#jobs-pill'];

function setBackgroundInert(on) {
  DRAWER_BACKGROUND_REGIONS.forEach(function(sel) {
    const el = document.querySelector(sel);
    if (el) el.inert = on;
  });
}

function anyDrawerOpen() {
  return ['settings', 'jobs', 'review'].some(function(n) {
    const d = document.getElementById('panel-' + n);
    return d && d.classList.contains('open');
  });
}

/* First keyboard-focusable element inside a container (used to move focus into a drawer). */
function firstFocusable(container) {
  return container.querySelector(
    'button:not([disabled]), [href], input:not([disabled]), select, textarea, [tabindex]:not([tabindex="-1"])'
  );
}

/* Open a drawer overlay (Settings or Activity) over the workspace. Traps focus inside it by
   making the background `inert`, so Tab cycles only within the drawer. Only one drawer is
   ever open: any already-open sibling is hidden first (covers an in-flight job resolving
   into switchTab('jobs') while Settings is open). Focus moves into the drawer on open and
   is restored to the opener on close (see closeDrawer). */
function openDrawer(name) {
  const drawer = document.getElementById('panel-' + name);
  if (!drawer) return;
  // Capture "was a drawer already open" BEFORE we hide any sibling, so switching drawers
  // keeps the original opener and doesn't re-inert (which is already on).
  const wasOpen = anyDrawerOpen();
  ['settings', 'jobs', 'review'].forEach(function(n) {
    if (n !== name) {
      const other = document.getElementById('panel-' + n);
      if (other) other.classList.remove('open');
    }
  });
  if (!wasOpen) {
    lastDrawerOpener = document.activeElement;
    setBackgroundInert(true);
  }
  drawer.classList.add('open');
  if (name === 'settings') loadSettings();
  if (name === 'jobs') {
    // Legacy job rendering guards on currentTab === 'jobs'; set it while open.
    currentTab = 'jobs';
    renderJobsPage();
    const pill = document.getElementById('jobs-pill');
    if (pill) pill.hidden = true;
  }
  if (name === 'review') loadReview();
  // Move focus into the drawer (first focusable — the close button).
  const target = firstFocusable(drawer);
  if (target) target.focus();
}

/* Close a drawer; restore the logical tab to the underlying workspace view. When the last
   drawer closes, lift the background `inert` and restore focus to the opener. */
function closeDrawer(name) {
  const drawer = document.getElementById('panel-' + name);
  if (drawer) drawer.classList.remove('open');
  if (name === 'jobs') {
    currentTab = currentView; // stop renderJobsPage from re-running
    if (typeof updateJobsPill === 'function') updateJobsPill();
  }
  if (!anyDrawerOpen()) {
    setBackgroundInert(false);
    if (lastDrawerOpener && typeof lastDrawerOpener.focus === 'function') {
      lastDrawerOpener.focus();
    }
    lastDrawerOpener = null;
  }
}

/* Esc closes whichever drawer is open. */
document.addEventListener('keydown', function(e) {
  if (e.key !== 'Escape') return;
  ['settings','jobs','review'].forEach(function(name) {
    const d = document.getElementById('panel-' + name);
    if (d && d.classList.contains('open')) closeDrawer(name);
  });
});

/* WAI-ARIA tablist keyboard support: the view tabs and Map sub-tabs already carry
   role="tab"/aria-selected + click handlers, but only respond to the mouse. Wire the
   arrow keys (and Home/End) the pattern expects: move focus to the adjacent tab and
   activate it via its existing click handler (automatic activation). */
function initTablistKeys() {
  document.querySelectorAll('[role="tablist"]').forEach(function(list) {
    list.addEventListener('keydown', function(e) {
      const tabs = Array.prototype.slice.call(list.querySelectorAll('[role="tab"]'));
      const idx = tabs.indexOf(document.activeElement);
      if (idx < 0) return;
      let next = null;
      if (e.key === 'ArrowRight' || e.key === 'ArrowDown') next = tabs[(idx + 1) % tabs.length];
      else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') next = tabs[(idx - 1 + tabs.length) % tabs.length];
      else if (e.key === 'Home') next = tabs[0];
      else if (e.key === 'End') next = tabs[tabs.length - 1];
      else return;
      e.preventDefault();
      next.focus();
      next.click(); // activate — mirrors a mouse click (switchTab / switchMapView)
    });
  });
}
initTablistKeys();

