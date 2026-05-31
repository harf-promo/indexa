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
    if (btn) btn.classList.toggle('active', t === tab);
    const panel = document.getElementById('panel-' + t);
    if (panel) panel.classList.toggle('active', t === tab);
  });
  const sv = document.getElementById('summary-view');
  if (sv) sv.style.display = (tab === 'tree' && selectedPath !== null) ? 'block' : '';
  if (tab === 'map') loadMap();
}

/* Open a drawer overlay (Settings or Activity) over the workspace. */
function openDrawer(name) {
  const drawer = document.getElementById('panel-' + name);
  if (!drawer) return;
  drawer.classList.add('open');
  if (name === 'settings') loadSettings();
  if (name === 'jobs') {
    // Legacy job rendering guards on currentTab === 'jobs'; set it while open.
    currentTab = 'jobs';
    renderJobsPage();
    const pill = document.getElementById('jobs-pill');
    if (pill) pill.hidden = true;
  }
}

/* Close a drawer; restore the logical tab to the underlying workspace view. */
function closeDrawer(name) {
  const drawer = document.getElementById('panel-' + name);
  if (drawer) drawer.classList.remove('open');
  if (name === 'jobs') {
    currentTab = currentView; // stop renderJobsPage from re-running
    if (typeof updateJobsPill === 'function') updateJobsPill();
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

