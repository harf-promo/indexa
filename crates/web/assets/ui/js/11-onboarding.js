'use strict';

/* ── First-run onboarding ──
   The index is "empty" when it has no roots. On an empty index we reveal the guided
   first-run steps in the Context panel (#welcome-empty) and land the user there instead
   of the Ask view (whose copy assumes context already exists). This is derived from live
   state every load — no localStorage flag — so it self-dismisses the moment a folder is
   added (index becomes non-empty) and never nags a populated index. */

/* Toggle the Context-panel welcome between the populated default and the empty-index
   guidance. Safe to call before either node exists (no-ops on missing nodes). */
function applyEmptyState(isEmpty) {
  const empty = document.getElementById('welcome-empty');
  const def = document.getElementById('welcome-default');
  if (empty) empty.hidden = !isEmpty;
  if (def) def.hidden = isEmpty;
}

/* Detect an empty index and, if so, switch to the Context view and show the guided
   steps. On a network error we leave the default (populated) behavior untouched so a
   transient blip never flashes onboarding at an established user. */
async function detectEmptyAndOnboard() {
  let isEmpty = false;
  try {
    const r = await fetch('/api/roots');
    // A store error returns a 500 whose body is the JSON object {error:…}, which parses
    // fine — so bail on !r.ok rather than trust the body, and treat anything that isn't a
    // genuine empty array as "not empty". Never flash onboarding at an established user.
    if (!r.ok) return;
    const roots = await r.json();
    isEmpty = Array.isArray(roots) && roots.length === 0;
  } catch (e) {
    return; // network/parse error → leave the populated-index default (init landed on Ask)
  }
  if (isEmpty) {
    applyEmptyState(true);
    // Don't steal the tab from a valid deep link (v0.37); show the empty banner regardless.
    if (!window.__indexaHashRestored) switchTab('tree');
  }
}

/* Show a "context ready" completion state in the welcome panel the first time a
   deep/index job finishes. Replaces the default welcome copy with action prompts.
   Called from the job SSE handler (04-jobs-views.js) on kind=deep/index done.
   Self-dismisses after 10 s or on any user action. */
function onContextReady(folderName) {
  var def = document.getElementById('welcome-default');
  if (!def || def.hidden) return; // already viewing something else or onboarding
  def.innerHTML =
    '<h2>Context ready! ✓</h2>' +
    '<p>Deep context for <strong>' + escapeHtml(folderName) + '</strong> is built.' +
    ' Try one of these:</p>' +
    '<div class="onboard-actions" style="flex-direction:column;align-items:flex-start;gap:8px">' +
    '<button class="onboard-cta" onclick="switchTab(\'chat\');this.closest(\'#welcome-default\').innerHTML=\'\'" >' + ICO_CHAT + ' Ask a question about your files</button>' +
    '<button class="btn-sm" onclick="doExport(\'\',\'xml\')" style="margin-left:0">' + ICO_DOWNLOAD + ' Export context for your AI tool</button>' +
    '<button class="btn-sm" onclick="this.closest(\'#welcome-default\').innerHTML=\'\'" style="margin-left:0">Browse folders →</button>' +
    '</div>';
  // Auto-dismiss after 10 s (clear the completion copy, don't re-flash the full onboarding)
  setTimeout(function() {
    var el = document.getElementById('welcome-default');
    if (el && el.querySelector('.onboard-cta')) el.innerHTML = '';
  }, 10000);
}

/* One row for a project returned by /api/projects: name, kind, an "N/M folders" coverage
   readout when the API actually returned nonzero totals (M4 — covered/total were computed
   and shipped over the wire but had no consumer; this is the consumer), and a Build context
   button. Shared by the welcome sidebar list and the multi-project chooser modal (H3) so
   both surfaces read the same real numbers instead of duplicating the row markup.
   `beforeBuild`, if given, runs before the build call fires (the modal uses it to close
   itself first). */
function buildProjectRow(p, beforeBuild) {  // eslint-disable-line no-unused-vars
  var row = document.createElement('div');
  row.className = 'welcome-project-row';
  var label = document.createElement('span');
  label.className = 'welcome-project-name';
  label.textContent = p.name;
  var kind = document.createElement('span');
  kind.className = 'welcome-project-kind';
  var covText = (p.total > 0) ? ((p.covered || 0) + '/' + p.total + ' folders') : '';
  kind.textContent = [p.app_name, covText].filter(Boolean).join(' \xb7 ');
  var btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'btn-sm';
  btn.textContent = 'Build context';
  btn.addEventListener('click', function () {
    if (typeof beforeBuild === 'function') beforeBuild();
    if (typeof fireBuildContext === 'function') {
      fireBuildContext(p.path, {
        kind: 'dir',
        chunk_count: p.chunk_count,
        covered: p.covered,
        total: p.total,
        summary_state: p.has_summary ? 'done' : null,
      });
    } else if (typeof fireJob === 'function') {
      fireJob(p.chunk_count > 0 ? 'summarize' : 'index', p.path);
    }
  });
  row.appendChild(label);
  row.appendChild(kind);
  row.appendChild(btn);
  return row;
}

/* Populate the welcome project list: uncovered detected apps get a Build context button. */
async function loadWelcomeProjects() {
  var slot = document.getElementById('welcome-projects');
  if (!slot) return;
  try {
    var r = await fetch('/api/projects');
    if (!r.ok) { slot.hidden = true; return; }
    var projects = await r.json();
    if (!Array.isArray(projects) || !projects.length) { slot.hidden = true; return; }
    var uncovered = projects.filter(function (p) { return !p.has_summary; });
    var n = projects.length;
    var missing = uncovered.length;
    var head = document.createElement('p');
    head.className = 'welcome-projects-head';
    head.textContent = missing === 0
      ? n + ' project' + (n === 1 ? '' : 's') + ' have summaries.'
      : missing + ' of ' + n + ' project' + (n === 1 ? '' : 's') + ' have no summaries.';
    slot.textContent = '';
    slot.appendChild(head);
    uncovered.slice(0, 24).forEach(function (p) {
      slot.appendChild(buildProjectRow(p));
    });
    slot.hidden = false;
  } catch (_) {
    slot.hidden = true;
  }
}

document.addEventListener('DOMContentLoaded', function () {
  if (typeof loadWelcomeProjects === 'function') loadWelcomeProjects();
});
