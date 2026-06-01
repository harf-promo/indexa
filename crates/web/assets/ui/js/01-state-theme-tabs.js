'use strict';

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

/* ── Theme ── */
(function initTheme() {
  const saved = localStorage.getItem('indexa_theme') || 'dark';
  document.documentElement.setAttribute('data-theme', saved);
  const btn = document.getElementById('theme-toggle');
  if (btn) btn.textContent = saved === 'light' ? '🌙' : '🌗';
})();

function toggleTheme() {
  const current = document.documentElement.getAttribute('data-theme') || 'dark';
  const next = current === 'dark' ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  localStorage.setItem('indexa_theme', next);
  const btn = document.getElementById('theme-toggle');
  if (btn) btn.textContent = next === 'light' ? '🌙' : '🌗';
}

/* ── Navigation ──
   'tree' | 'chat' | 'map'  → workspace views (in-place toggle)
   'settings' | 'jobs'      → drawers opened over the workspace
   Single entry point so every existing caller (showSummary→'tree', doAsk→'chat',
   fireJob→'jobs', the pill, the gear) keeps working. */
function switchTab(tab) {
  if (tab === 'settings' || tab === 'jobs') { openDrawer(tab); return; }

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
  if (tab === 'map') loadMap();
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
  return ['settings', 'jobs'].some(function(n) {
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
  ['settings', 'jobs'].forEach(function(n) {
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
  ['settings','jobs'].forEach(function(name) {
    const d = document.getElementById('panel-' + name);
    if (d && d.classList.contains('open')) closeDrawer(name);
  });
});

