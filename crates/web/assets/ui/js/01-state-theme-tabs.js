'use strict';

/* ── State ── */
let currentTab = 'chat';
let selectedPath = null;
const expandedPaths = new Set();

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

/* ── Tab switching ── */
function switchTab(tab) {
  currentTab = tab;
  ['tree','chat','map','settings','jobs'].forEach(function(t) {
    const btn = document.getElementById('tab-' + t);
    if (btn) btn.classList.toggle('active', t === tab);
    const panel = document.getElementById('panel-' + t);
    if (panel) panel.classList.toggle('active', t === tab);
  });
  const sv = document.getElementById('summary-view');
  if (sv) sv.style.display = (tab === 'tree' && selectedPath !== null) ? 'block' : '';
  if (tab === 'settings') loadSettings();
  if (tab === 'map') loadMap();
  if (tab === 'jobs') renderJobsPage();
  // Hide the pill when the jobs tab is active
  const pill = document.getElementById('jobs-pill');
  if (pill) pill.hidden = (tab === 'jobs');
}

