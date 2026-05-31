/* ── Job helpers ── */
async function fireJob(kind, path) {
  // Pre-flight memory-fit gate for the model-loading kinds ("ask me first"):
  // summarize loads the dir-roll-up model; index runs deep + summarize. scan/deep
  // load no heavy model, so they skip the gate.
  let modelParams = '';
  if (kind === 'summarize' || kind === 'index') {
    const choice = await modelFitGate(path);
    if (choice === null) return; // user cancelled the build
    modelParams = choice; // '' (configured) or a recommended-model override
  }
  const r = await fetch('/api/jobs/' + kind + '?path=' + encodeURIComponent(path) + modelParams, { method: 'POST' });
  const d = await r.json();
  subscribeJob(d.job_id, path, kind);
  // Switch to jobs tab so user can watch progress
  switchTab('jobs');
}

/* Calm, STATIC per-row context-coverage glyph. Replaces the old per-row pending
   strobe: instead of a pulsing ⏳ on every folder during a subtree build, each dir
   shows where its subtree stands — ● built · ◐ partly built · ○ none · ✗ failed.
   Files (total === 0) and un-rolled-up roots carry no glyph. No animation. */
function coverageGlyph(node) {
  if (node.summary_state === 'failed') {
    return '<span class="cov-glyph cov-failed" title="Summary failed">✗</span>';
  }
  const total = node.total || 0;
  if (total <= 0) return '';
  const covered = node.covered || 0;
  if (covered >= total) {
    return '<span class="cov-glyph cov-full" title="Context built (' + covered + '/' + total + ')">●</span>';
  }
  if (covered > 0) {
    return '<span class="cov-glyph cov-partial" title="Partly built (' + covered + '/' + total + ')">◐</span>';
  }
  return '<span class="cov-glyph cov-none" title="No context yet (0/' + total + ')">○</span>';
}

function buildTreeNode(node) {
  const wrap = document.createElement('div');
  wrap.className = 'tree-node';
  wrap.dataset.path = node.path;

  const isDir = node.kind === 'dir';
  const icon = isDir ? '📁' : '📄';
  // Stash the subtree coverage rollup so the summary header can show a "context: N%"
  // chip for this path without a second request (see coverageByPath / renderSummary).
  coverageByPath[node.path] = { covered: node.covered || 0, partial: node.partial || 0, total: node.total || 0 };
  const badge = coverageGlyph(node);
  // One calm, determinate count per ACTIVE subtree (something still queued), instead of
  // N pulsing children. Static until the next tree refresh — live updates arrive in PR-4.
  const coverageCount = (isDir && node.partial > 0 && node.total > 0)
    ? '<span class="cov-count" title="Folders with context built in this subtree">' +
        (node.covered || 0) + '/' + node.total + '</span>'
    : '';
  const toggle = isDir ? '<span class="tree-toggle">▸</span>' : '<span class="tree-toggle"></span>';

  const countSuffix = (isDir && (node.file_count > 0 || node.chunk_count > 0))
    ? '<span style="color:var(--muted);font-size:10px;margin-left:4px;flex-shrink:0">' +
        (node.file_count > 0 ? node.file_count + ' files' : '') +
        (node.file_count > 0 && node.chunk_count > 0 ? ' \xb7 ' : '') +
        (node.chunk_count > 0 ? node.chunk_count + ' chunks' : '') +
      '</span>'
    : '';

  const row = document.createElement('div');
  row.className = 'tree-node-row' + (node.path === selectedPath ? ' selected' : '');
  row.innerHTML = toggle + '<span class="tree-icon">' + icon + '</span>' +
    '<span class="tree-label" title="' + escapeAttr(node.path) + '">' + escapeHtml(node.name) + '</span>' +
    countSuffix +
    coverageCount +
    badge +
    '<span class="tree-row-actions">' +
    '<button data-act="scan" title="Re-scan">&#x21BB;</button>' +
    '<button data-act="deep" title="Deep index">&#x26A1;</button>' +
    '<button data-act="summarize" title="Summarize">&#x1F4DD;</button>' +
    '<button data-act="remove" title="Remove from index">&#x1F5D1;</button>' +
    '</span>';

  row.querySelectorAll('.tree-row-actions button').forEach(function(btn) {
    btn.addEventListener('click', async function(e) {
      e.stopPropagation();
      const act = btn.dataset.act;
      if (act === 'remove') {
        const label = node.path.split('/').pop() || node.path;
        if (!(await confirmModal('Remove ‹' + label + '› from the index?\nFiles on disk are not deleted.', 'Remove'))) return;
        try {
          await fetch('/api/entry?path=' + encodeURIComponent(node.path), { method: 'DELETE' });
          expandedPaths.delete(node.path);
          refreshTree();
          loadStats();
        } catch(err) { toast('Remove failed: ' + err.message, 'error'); }
      } else {
        try {
          await fireJob(act, node.path);
        } catch(err) { toast('Failed to start job: ' + err.message, 'error'); }
      }
    });
  });

  const childContainer = document.createElement('div');
  childContainer.className = 'tree-children';
  childContainer.style.display = 'none';

  row.querySelector('.tree-label').addEventListener('click', function(e) {
    if (e.altKey || e.metaKey) {
      e.stopPropagation();
      const inp = document.getElementById('search-input');
      inp.value = node.path + '/';
      document.getElementById('search-clear').style.display = '';
      doSearch(node.path + '/');
    }
  });

  row.addEventListener('click', function(e) {
    e.stopPropagation();
    document.querySelectorAll('.tree-node-row.selected').forEach(function(el) { el.classList.remove('selected'); });
    row.classList.add('selected');
    selectedPath = node.path;
    showSummary(node.path);

    if (isDir) {
      const isExpanded = expandedPaths.has(node.path);
      if (isExpanded) {
        expandedPaths.delete(node.path);
        childContainer.style.display = 'none';
        row.querySelector('.tree-toggle').textContent = '▸';
      } else {
        expandedPaths.add(node.path);
        childContainer.style.display = 'block';
        row.querySelector('.tree-toggle').textContent = '▾';
        if (!childContainer.dataset.loaded) {
          childContainer.dataset.loaded = '1';
          loadTreeLevel(node.path, childContainer);
        }
      }
    }
  });

  wrap.appendChild(row);
  if (isDir) wrap.appendChild(childContainer);
  return wrap;
}

async function initTree() {
  const list = document.getElementById('tree-list');
  list.innerHTML = '<div style="padding:8px 12px;color:var(--muted);font-size:12px">Loading…</div>';
  try {
    const r = await fetch('/api/roots');
    const roots = await r.json();
    if (!roots.length) {
      list.innerHTML = '<div class="empty-state">No indexed roots yet.<br><span class="cta-link" onclick="openAddRoot()">+ Add Root</span> to get started, or run <code>indexa scan &lt;path&gt;</code> in your terminal.</div>';
      return;
    }
    list.innerHTML = '';
    roots.forEach(function(root) {
      list.appendChild(buildTreeNode({path: root.path, name: root.name, kind: 'dir', summary_state: null}));
    });
  } catch(e) {
    list.innerHTML = '<div style="padding:8px 12px;color:var(--red);font-size:12px">Error loading tree</div>';
  }
}

/* Expand a single already-rendered tree node by path (loads its children). */
async function expandNodeByPath(path) {
  const sel = '.tree-node[data-path="' + (window.CSS && CSS.escape ? CSS.escape(path) : path) + '"]';
  const wrap = document.querySelector(sel);
  if (!wrap) return;
  const row = wrap.querySelector('.tree-node-row');
  const childContainer = wrap.querySelector('.tree-children');
  if (!row || !childContainer) return;
  expandedPaths.add(path);
  childContainer.style.display = 'block';
  const toggle = row.querySelector('.tree-toggle');
  if (toggle) toggle.textContent = '▾';
  if (!childContainer.dataset.loaded) {
    childContainer.dataset.loaded = '1';
    await loadTreeLevel(path, childContainer);
  }
}

/* Rebuild the tree while preserving expanded folders and scroll position.
   Use this after a job completes instead of initTree(), which collapses everything. */
async function refreshTree() {
  const list = document.getElementById('tree-list');
  const prevScroll = list ? list.scrollTop : 0;
  // Snapshot the open folders, shallowest-first so parents expand before children.
  const toRestore = Array.from(expandedPaths).sort(function(a, b) {
    return a.split('/').length - b.split('/').length;
  });
  expandedPaths.clear();
  await initTree();
  for (const p of toRestore) {
    await expandNodeByPath(p); // parent is already in the DOM by the time we reach a child
  }
  if (list) list.scrollTop = prevScroll;
}

/* ── Search ── */
var _searchTimer = null;
function onSearchInput(val) {
  const clearBtn = document.getElementById('search-clear');
  if (clearBtn) clearBtn.style.display = val ? '' : 'none';
  clearTimeout(_searchTimer);
  if (!val) { initTree(); return; }
  _searchTimer = setTimeout(function() { doSearch(val); }, 250);
}

function clearSearchInput() {
  const inp = document.getElementById('search-input');
  if (inp) inp.value = '';
  const clearBtn = document.getElementById('search-clear');
  if (clearBtn) clearBtn.style.display = 'none';
  initTree();
}

async function doSearch(query) {
  const list = document.getElementById('tree-list');
  list.innerHTML = '<div style="padding:8px 12px;color:var(--muted);font-size:12px">Searching…</div>';
  try {
    const r = await fetch('/api/search?q=' + encodeURIComponent(query));
    const results = await r.json();
    if (!results.length) {
      list.innerHTML = '<div style="padding:8px 12px;color:var(--muted);font-size:12px">No results for "' + escapeHtml(query) + '"</div>';
      return;
    }
    list.innerHTML = '';
    results.forEach(function(node) { list.appendChild(buildTreeNode(node)); });
  } catch(e) {
    list.innerHTML = '<div style="padding:8px 12px;color:var(--red);font-size:12px">Search error</div>';
  }
}

/* ══════════════════════════════════════════════════════════════════
   JOBS — data model + render system
   ══════════════════════════════════════════════════════════════════ */

/**
 * Central store for all job state. Keyed by jobId (string).
 * Each entry is never deleted on completion — user must Dismiss it.
 * Shape: { es, _retries, status, kind, path, startedAt, snapshot,
 *          lastProgress, warnings, warningOverflow, stageCounts,
 *          llm, failedEvent, summary }
 */
var activeJobs = {};

/** Currently selected jobId in the Jobs tab detail pane. */
var selectedJobId = null;

/** Filter for the master list: 'all' | 'running' | 'done' | 'failed' */
var jobsFilter = 'all';

/** Whether the AI output panel in the detail pane is open. */
var detailAiOpen = false;

/* rAF batching for high-frequency updates */
var _dirtyJobs = {};   // jobId → true when state changed
var _rafPending = false;

function _markDirty(jobId) {
  _dirtyJobs[jobId] = true;
  if (!_rafPending) { _rafPending = true; requestAnimationFrame(_drain); }
}

function _drain() {
  _rafPending = false;
  var dirty = Object.keys(_dirtyJobs);
  _dirtyJobs = {};
  dirty.forEach(function(jid) {
    renderJobCard(jid);
    if (jid === selectedJobId) renderJobDetail(jid);
  });
  updateJobsPill();
  updateJobsTabBadge();
}

