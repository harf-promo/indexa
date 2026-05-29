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

/* ── Stats ── */
async function loadStats() {
  try {
    const r = await fetch('/api/stats');
    const d = await r.json();
    document.getElementById('stats').textContent =
      d.entries.toLocaleString() + ' files \xb7 ' + d.chunks.toLocaleString() + ' chunks';
  } catch(e) { document.getElementById('stats').textContent = 'No index yet'; }
}

/* ── Tree ── */
async function loadTreeLevel(parentPath, container) {
  container.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:12px">Loading…</div>';
  try {
    const url = '/api/tree?path=' + encodeURIComponent(parentPath);
    const r = await fetch(url);
    const nodes = await r.json();
    if (!nodes.length) {
      container.innerHTML = '<div style="padding:6px 12px;color:var(--muted);font-size:12px">Empty</div>';
      return;
    }
    container.innerHTML = '';
    nodes.forEach(function(node) { container.appendChild(buildTreeNode(node)); });
  } catch(e) {
    container.innerHTML = '<div style="padding:6px 12px;color:var(--red);font-size:12px">Error loading</div>';
  }
}

/* ── Job helpers ── */
async function fireJob(kind, path) {
  const r = await fetch('/api/jobs/' + kind + '?path=' + encodeURIComponent(path), { method: 'POST' });
  const d = await r.json();
  subscribeJob(d.job_id, path, kind);
  // Switch to jobs tab so user can watch progress
  switchTab('jobs');
}

function badgeFor(state) {
  if (!state) return '';
  if (state === 'done') return '<span class="tree-badge done" title="Summarized">✓</span>';
  if (state === 'failed') return '<span class="tree-badge failed" title="Summary failed">✗</span>';
  return '<span class="tree-badge pending" title="Summary pending">⏳</span>';
}

function buildTreeNode(node) {
  const wrap = document.createElement('div');
  wrap.className = 'tree-node';
  wrap.dataset.path = node.path;

  const isDir = node.kind === 'dir';
  const icon = isDir ? '📁' : '📄';
  const badge = badgeFor(node.summary_state);
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
        if (!confirm('Remove ‹' + label + '› from the index?\nFiles on disk are not deleted.')) return;
        try {
          await fetch('/api/entry?path=' + encodeURIComponent(node.path), { method: 'DELETE' });
          initTree();
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

/* ── localStorage helpers ── */
function _saveActiveJob(jobId, path) {
  try {
    const stored = JSON.parse(localStorage.getItem('indexa.activeJobs') || '{}');
    stored[jobId] = { id: jobId, path: path, saved_at: Date.now() };
    localStorage.setItem('indexa.activeJobs', JSON.stringify(stored));
  } catch(_) {}
}
function _removeActiveJob(jobId) {
  try {
    const stored = JSON.parse(localStorage.getItem('indexa.activeJobs') || '{}');
    delete stored[jobId];
    localStorage.setItem('indexa.activeJobs', JSON.stringify(stored));
  } catch(_) {}
}

/* ── Subscribe to a job's SSE stream ── */
function subscribeJob(jobId, path, kind) {
  if (!activeJobs[jobId]) {
    activeJobs[jobId] = {
      es: null, _retries: 0,
      status: 'running',
      kind: kind || '?', path: path || jobId, startedAt: Date.now(),
      snapshot: null, lastProgress: null,
      warnings: [], warningOverflow: 0, stageCounts: {},
      llm: { path: null, label: '', text: '' },
      failedEvent: null, summary: null
    };
  }
  _saveActiveJob(jobId, path);

  const job = activeJobs[jobId];
  const es = new EventSource('/api/jobs/' + jobId + '/events');
  job.es = es;

  es.onmessage = function(e) {
    try {
      const ev = JSON.parse(e.data);
      const j = activeJobs[jobId];
      if (!j) return;

      if (ev.type === 'start') {
        j.kind = ev.kind || '?';
        j.path = ev.path || j.path;
        j.status = 'running';
        j._retries = 0;
        _markDirty(jobId);

      } else if (ev.type === 'snapshot') {
        j.snapshot = { count: ev.count, bytes: ev.bytes };
        _markDirty(jobId);

      } else if (ev.type === 'progress') {
        j.lastProgress = ev;
        _markDirty(jobId);

      } else if (ev.type === 'done') {
        j.status = 'done';
        j.summary = ev.summary || '';
        if (j.es) { j.es.close(); j.es = null; }
        _removeActiveJob(jobId);
        playPing('ok');
        _markDirty(jobId);
        // Refresh tree and stats in background
        setTimeout(function() { initTree(); loadStats(); }, 500);

      } else if (ev.type === 'failed') {
        j.status = 'failed';
        j.failedEvent = ev;
        if (j.es) { j.es.close(); j.es = null; }
        _removeActiveJob(jobId);
        playPing('err');
        _markDirty(jobId);

      } else if (ev.type === 'warning') {
        const MAX_WARNINGS = 500;
        if (j.warnings.length < MAX_WARNINGS) {
          j.warnings.push(ev);
        } else {
          j.warningOverflow++;
        }
        j.stageCounts[ev.stage] = (j.stageCounts[ev.stage] || 0) + 1;
        _markDirty(jobId);

      } else if (ev.type === 'llm_fragment') {
        const reset = j.llm.path !== ev.item_path;
        if (reset) {
          j.llm.path = ev.item_path;
          j.llm.text = '';
        }
        j.llm.label = ev.model + ' \xb7 ' + ev.stage;
        j.llm.text += ev.fragment;
        // Cap at 8 KB client-side
        if (j.llm.text.length > 8192) {
          j.llm.text = j.llm.text.slice(j.llm.text.length - 8192);
        }
        _markDirty(jobId);
      }
    } catch(_) {}
  };

  es.onerror = function() {
    const j = activeJobs[jobId];
    if (!j) return;
    if (j.status === 'done' || j.status === 'failed') return;

    es.close();
    j.es = null;

    fetch('/api/jobs/' + jobId).then(function(r) {
      if (r.status === 404) {
        // Server evicted the job (60 s after done)
        return;
      }
      const retries = j._retries || 0;
      const delays = [250, 500, 1000, 2000, 4000];
      const delay = delays[Math.min(retries, delays.length - 1)];
      j.status = 'reconnecting';
      _markDirty(jobId);
      setTimeout(function() {
        if (!activeJobs[jobId]) return;
        j._retries = retries + 1;
        subscribeJob(jobId, j.path);
      }, delay);
    }).catch(function() {});
  };
}

/* ── Reconnect in-flight jobs on page load ── */
async function reconnectInFlightJobs() {
  const lsJobs = {};
  try {
    const stored = JSON.parse(localStorage.getItem('indexa.activeJobs') || '{}');
    Object.assign(lsJobs, stored);
  } catch(_) {}

  try {
    const r = await fetch('/api/jobs');
    const jobs = await r.json();
    jobs.forEach(function(j) {
      if (j.status === 'running' || j.status === 'done' || j.status === 'failed') {
        if (!activeJobs[j.job_id]) {
          subscribeJob(j.job_id, j.path, j.kind);
          // Seed the known status from the server so we don't flicker to 'running'
          if (activeJobs[j.job_id]) activeJobs[j.job_id].status = j.status;
        }
      }
    });
    Object.keys(lsJobs).forEach(function(id) {
      const serverJob = jobs.find(function(j) { return j.job_id === id; });
      if (!serverJob && Date.now() - (lsJobs[id].saved_at || 0) > 90000) {
        _removeActiveJob(id);
      }
    });
  } catch(_) {}
}

/* ── Jobs master list ── */
function setJobsFilter(f) {
  jobsFilter = f;
  document.querySelectorAll('.jobs-filter').forEach(function(btn) {
    btn.classList.toggle('active', btn.dataset.filter === f);
  });
  renderJobsPage();
}

function clearFinishedJobs() {
  Object.keys(activeJobs).forEach(function(id) {
    const j = activeJobs[id];
    if (j.status === 'done' || j.status === 'failed') {
      if (j.es) { j.es.close(); }
      delete activeJobs[id];
    }
  });
  if (selectedJobId && !activeJobs[selectedJobId]) selectedJobId = null;
  renderJobsPage();
  updateJobsPill();
  updateJobsTabBadge();
}

function dismissSelectedJob() {
  if (!selectedJobId) return;
  const j = activeJobs[selectedJobId];
  if (j && j.es) j.es.close();
  delete activeJobs[selectedJobId];
  selectedJobId = null;
  renderJobsPage();
  updateJobsPill();
  updateJobsTabBadge();
}

function selectJob(jobId) {
  selectedJobId = jobId;
  document.querySelectorAll('.jobs-card').forEach(function(c) {
    c.classList.toggle('selected', c.dataset.jobId === jobId);
  });
  renderJobDetail(jobId);
}

function renderJobsPage() {
  if (currentTab !== 'jobs') return;
  const masterList = document.getElementById('jobs-master-list');
  const empty = document.getElementById('jobs-empty');
  if (!masterList) return;

  const all = Object.values(activeJobs).sort(function(a, b) { return b.startedAt - a.startedAt; });
  const visible = all.filter(function(j) {
    if (jobsFilter === 'all') return true;
    if (jobsFilter === 'running') return j.status === 'running' || j.status === 'reconnecting';
    return j.status === jobsFilter;
  });

  if (empty) empty.hidden = visible.length > 0;

  // Build set of visible jobIds for O(1) lookup below.
  var visibleIds = new Set(visible.map(function(j) {
    return Object.keys(activeJobs).find(function(id) { return activeJobs[id] === j; });
  }).filter(Boolean));

  // Remove cards for jobs that are deleted OR filtered out.
  masterList.querySelectorAll('.jobs-card').forEach(function(card) {
    if (!visibleIds.has(card.dataset.jobId)) card.remove();
  });

  visible.forEach(function(j) {
    const jobId = Object.keys(activeJobs).find(function(id) { return activeJobs[id] === j; });
    if (!jobId) return;
    let card = masterList.querySelector('.jobs-card[data-job-id="' + jobId + '"]');
    if (!card) {
      card = document.createElement('div');
      card.className = 'jobs-card';
      card.dataset.jobId = jobId;
      card.addEventListener('click', function() { selectJob(jobId); });
      masterList.prepend(card);
    }
    _renderCardContent(card, jobId, j);
  });

  // Auto-select: prefer a running job that matches the current filter.
  if (!selectedJobId || !activeJobs[selectedJobId] || !visibleIds.has(selectedJobId)) {
    const firstVisible = visibleIds.size > 0 ? visibleIds.values().next().value : null;
    const running = [...visibleIds].find(function(id) {
      var j = activeJobs[id];
      return j && (j.status === 'running' || j.status === 'reconnecting');
    });
    const toSelect = running || firstVisible;
    if (toSelect) { selectJob(toSelect); }
    else {
      // No visible jobs → show placeholder
      const content = document.getElementById('jd-content');
      const placeholder = document.getElementById('jd-placeholder');
      if (content) content.hidden = true;
      if (placeholder) placeholder.hidden = false;
    }
  } else {
    renderJobDetail(selectedJobId);
  }
}

function renderJobCard(jobId) {
  const j = activeJobs[jobId];
  if (!j) return;
  const card = document.querySelector('.jobs-card[data-job-id="' + jobId + '"]');
  if (card) {
    _renderCardContent(card, jobId, j);
    return;
  }
  // If the jobs tab is open, add it
  if (currentTab === 'jobs') renderJobsPage();
}

function _renderCardContent(card, jobId, j) {
  card.classList.toggle('selected', jobId === selectedJobId);
  card.classList.toggle('running', j.status === 'running' || j.status === 'reconnecting');
  card.classList.toggle('done', j.status === 'done');
  card.classList.toggle('failed', j.status === 'failed');

  const pathName = (j.path || '').split('/').filter(Boolean).pop() || j.path || jobId;
  const warnCount = j.warnings.length + j.warningOverflow;
  const warnBadge = warnCount > 0 ? '<span class="jc-warn-badge">⚠ ' + warnCount + '</span>' : '';
  let statusText = '';
  if (j.status === 'running' || j.status === 'reconnecting') {
    if (j.lastProgress) {
      statusText = j.lastProgress.current + '/' + j.lastProgress.total;
    } else {
      statusText = j.status === 'reconnecting' ? 'reconnecting…' : 'starting…';
    }
  } else if (j.status === 'done') {
    statusText = '✓ ' + (j.summary || 'done');
  } else if (j.status === 'failed') {
    statusText = '✗ failed';
  }

  card.innerHTML =
    '<div class="jc-header">' +
      '<span class="jc-kind">' + escapeHtml(j.kind) + '</span>' +
      '<span class="jc-path" title="' + escapeAttr(j.path) + '">' + escapeHtml(pathName) + '</span>' +
      warnBadge +
    '</div>' +
    '<div class="jc-status">' + escapeHtml(statusText) + '</div>';

  if (j.status === 'running' && j.lastProgress && j.lastProgress.total > 0) {
    card.innerHTML += '<progress class="jc-bar" value="' + j.lastProgress.current + '" max="' + j.lastProgress.total + '"></progress>';
  }

  card.onclick = function() { selectJob(jobId); };
}

/* ── Jobs detail pane ── */
var _elapsedInterval = null;

function renderJobDetail(jobId) {
  const j = activeJobs[jobId];
  const content = document.getElementById('jd-content');
  const placeholder = document.getElementById('jd-placeholder');
  if (!content || !placeholder) return;

  if (!j) {
    content.hidden = true;
    placeholder.hidden = false;
    return;
  }

  content.hidden = false;
  placeholder.hidden = true;

  // Header
  const kindEl = document.getElementById('jd-kind');
  const pathEl = document.getElementById('jd-path');
  const statusChip = document.getElementById('jd-status-chip');
  const copyBtn = document.getElementById('jd-copy-btn');

  if (kindEl) { kindEl.textContent = j.kind; kindEl.className = 'jd-kind-badge jd-kind-' + j.kind; }
  if (pathEl) { pathEl.textContent = j.path; pathEl.title = j.path; }

  if (statusChip) {
    let chipClass = 'jd-status-chip';
    let chipText = '';
    if (j.status === 'running' || j.status === 'reconnecting') {
      chipClass += ' running'; chipText = j.status === 'reconnecting' ? 'reconnecting' : 'running';
    } else if (j.status === 'done') { chipClass += ' done'; chipText = 'done'; }
    else if (j.status === 'failed') { chipClass += ' failed'; chipText = 'failed'; }
    statusChip.className = chipClass;
    statusChip.textContent = chipText;
  }

  if (copyBtn) copyBtn.style.display = j.failedEvent ? '' : 'none';

  // Elapsed timer
  clearInterval(_elapsedInterval);
  function updateElapsed() {
    const el = document.getElementById('jd-elapsed');
    if (!el) return;
    const secs = Math.floor((Date.now() - j.startedAt) / 1000);
    const m = Math.floor(secs / 60), s = secs % 60;
    el.textContent = m > 0 ? m + 'm ' + s + 's' : s + 's';
  }
  updateElapsed();
  if (j.status === 'running' || j.status === 'reconnecting') {
    _elapsedInterval = setInterval(updateElapsed, 1000);
  }

  // Progress
  const progressRow = document.getElementById('jd-progress-row');
  const bar = document.getElementById('jd-bar');
  const countEl = document.getElementById('jd-count');
  const speedEl = document.getElementById('jd-speed');
  const lp = j.lastProgress;
  if (progressRow) progressRow.hidden = (j.status === 'done' || j.status === 'failed');
  if (bar && lp && lp.total > 0) { bar.value = lp.current; bar.max = lp.total; }
  if (countEl && lp) countEl.textContent = lp.current + ' / ' + lp.total;
  if (speedEl && lp) {
    const parts = [];
    if (lp.items_per_sec && lp.items_per_sec > 0) parts.push(lp.items_per_sec.toFixed(1) + ' files/s');
    if (lp.eta_secs && lp.eta_secs > 0) {
      const eta = lp.eta_secs < 60 ? Math.round(lp.eta_secs) + 's' : Math.round(lp.eta_secs / 60) + 'm';
      parts.push('ETA ' + eta);
    }
    if (lp.note) parts.push(lp.note);
    speedEl.textContent = parts.join(' \xb7 ');
  }

  // Live AI section
  const liveSection = document.getElementById('jd-live');
  const liveFile = document.getElementById('jd-live-file');
  const liveModel = document.getElementById('jd-live-model');
  const aiPre = document.getElementById('jd-ai-pre');
  const hasLlm = j.llm && j.llm.text;
  if (liveSection) liveSection.hidden = !hasLlm && j.status !== 'running';
  if (liveFile && lp && lp.current_path) {
    const parts = lp.current_path.split('/');
    liveFile.textContent = parts.slice(-2).join('/');
    liveFile.title = lp.current_path;
  }
  if (liveModel && j.llm.label) liveModel.textContent = j.llm.label;
  if (aiPre && hasLlm) {
    aiPre.textContent = j.llm.text;
    if (detailAiOpen) aiPre.scrollTop = aiPre.scrollHeight;
  }
  if (aiPre) aiPre.hidden = !detailAiOpen;

  // Stats row
  const statsFiles = document.getElementById('jd-stats-files');
  const statsBytes = document.getElementById('jd-stats-bytes');
  if (statsFiles && j.snapshot) statsFiles.textContent = j.snapshot.count.toLocaleString() + ' items';
  if (statsBytes && j.snapshot && j.snapshot.bytes > 0) {
    const mb = (j.snapshot.bytes / 1024 / 1024).toFixed(1);
    statsBytes.textContent = mb + ' MB';
  }

  // Warnings summary badge
  const warnSummary = document.getElementById('jd-warn-summary');
  const totalWarns = j.warnings.length + j.warningOverflow;
  if (warnSummary) {
    warnSummary.textContent = totalWarns > 0 ? '⚠ ' + totalWarns + ' warning' + (totalWarns !== 1 ? 's' : '') : '';
    warnSummary.style.color = totalWarns > 0 ? 'var(--orange)' : '';
  }

  // Warnings section
  const warnSection = document.getElementById('jd-warnings-section');
  if (warnSection) warnSection.hidden = totalWarns === 0;
  if (totalWarns > 0) {
    const warnTitle = document.getElementById('jd-warn-title');
    if (warnTitle) warnTitle.textContent = '⚠ ' + totalWarns + (totalWarns !== 1 ? ' warnings' : ' warning');
    _populateWarnStageFilter(j);
    applyWarnFilter();
  }

  // Failed error section
  const errSection = document.getElementById('jd-error-section');
  if (errSection) errSection.hidden = !j.failedEvent;
  if (j.failedEvent) {
    const errMsg = document.getElementById('jd-error-msg');
    const errChain = document.getElementById('jd-error-chain');
    if (errMsg) errMsg.textContent = (j.failedEvent.stage ? '[' + j.failedEvent.stage + '] ' : '') + j.failedEvent.error;
    if (errChain && j.failedEvent.chain && j.failedEvent.chain.length > 1) {
      errChain.innerHTML = j.failedEvent.chain.map(function(c, i) {
        return '<div class="jd-error-chain-item">' + escapeHtml((i + 1) + '. ' + c) + '</div>';
      }).join('');
    }
  }
}

function _populateWarnStageFilter(j) {
  const sel = document.getElementById('jd-warn-stage-filter');
  if (!sel) return;
  const stages = Object.keys(j.stageCounts).sort();
  // Only rebuild the <select> when the SET of stages changes. Rebuilding on every
  // render tick collapses the dropdown while the user is trying to pick a value.
  const signature = stages.join('|');
  if (sel.dataset.stageSig === signature) return;
  sel.dataset.stageSig = signature;

  const current = sel.value;
  sel.innerHTML = '<option value="">All stages (' + (j.warnings.length + j.warningOverflow) + ')</option>';
  stages.forEach(function(s) {
    const opt = document.createElement('option');
    opt.value = s;
    opt.textContent = s + ' (' + j.stageCounts[s] + ')';
    sel.appendChild(opt);
  });
  if (current && stages.includes(current)) sel.value = current;
}

function applyWarnFilter() {
  if (!selectedJobId || !activeJobs[selectedJobId]) return;
  const j = activeJobs[selectedJobId];
  const stageFilter = (document.getElementById('jd-warn-stage-filter') || {}).value || '';
  const textFilter = ((document.getElementById('jd-warn-search') || {}).value || '').toLowerCase();
  const list = document.getElementById('jd-warn-list');
  if (!list) return;

  const filtered = j.warnings.filter(function(w) {
    if (stageFilter && w.stage !== stageFilter) return false;
    if (textFilter) {
      const hay = ((w.message || '') + ' ' + (w.item_path || '')).toLowerCase();
      if (hay.indexOf(textFilter) === -1) return false;
    }
    return true;
  });

  list.innerHTML = filtered.slice(0, 500).map(function(w) {
    const basename = w.item_path ? w.item_path.split('/').pop() : '';
    return '<div class="warn-row" onclick="this.classList.toggle(\'expanded\')">' +
      '<span class="warn-stage">' + escapeHtml(w.stage) + '</span>' +
      (basename ? '<span class="warn-path" title="' + escapeAttr(w.item_path) + '">' + escapeHtml(basename) + '</span>' : '') +
      '<span class="warn-msg">' + escapeHtml(w.message) + '</span>' +
      (w.item_path ? '<div class="warn-full-path">' + escapeHtml(w.item_path) + '</div>' : '') +
      '</div>';
  }).join('');

  if (filtered.length > 500) {
    list.innerHTML += '<div class="warn-overflow">… and ' + (filtered.length - 500) + ' more (refine your filter)</div>';
  }
  if (j.warningOverflow > 0) {
    list.innerHTML += '<div class="warn-overflow">' + j.warningOverflow + ' earlier warnings not shown (cap reached)</div>';
  }
}

function toggleDetailAi() {
  detailAiOpen = !detailAiOpen;
  const btn = document.getElementById('jd-ai-toggle');
  const pre = document.getElementById('jd-ai-pre');
  if (pre) pre.hidden = !detailAiOpen;
  if (btn) btn.classList.toggle('active', detailAiOpen);
  if (detailAiOpen && pre) pre.scrollTop = pre.scrollHeight;
}

/* ── Copy error report ── */
async function copyJobReport() {
  if (!selectedJobId || !activeJobs[selectedJobId]) return;
  const j = activeJobs[selectedJobId];
  if (!j.failedEvent) return;
  try {
    const r = await fetch('/api/logs/tail?lines=50');
    const d = await r.json();
    const ev = j.failedEvent;
    const chain = ev.chain && ev.chain.length
      ? '\nCaused by:\n' + ev.chain.map(function(c, i) { return (i + 1) + '. ' + c; }).join('\n')
      : '';
    const report = '**Indexa error report**\n' +
      '- Version: ' + (document.getElementById('app-version').textContent || '?') + '\n' +
      '- Job: ' + j.kind + ' ' + j.path + '\n' +
      '- Stage: ' + (ev.stage || '?') + '\n' +
      (ev.item_path ? '- Item: ' + ev.item_path + '\n' : '') +
      '- Error: ' + ev.error + chain + '\n\n' +
      '**Logs (last 50 lines)**\n' + (d.lines || '(no log file found)');
    await navigator.clipboard.writeText(report);
    toast('Error report copied to clipboard', 'info');
  } catch(err) { toast('Copy failed: ' + err.message, 'error'); }
}

/* ── Jobs mini pill ── */
function updateJobsPill() {
  const pill = document.getElementById('jobs-pill');
  const pillText = document.getElementById('jobs-pill-text');
  const pillDot = document.getElementById('jobs-pill-dot');
  if (!pill) return;

  const jobs = Object.values(activeJobs);
  const running = jobs.filter(function(j) { return j.status === 'running' || j.status === 'reconnecting'; });
  const failed = jobs.filter(function(j) { return j.status === 'failed'; });

  if (jobs.length === 0 || currentTab === 'jobs') {
    pill.hidden = true;
    return;
  }

  pill.hidden = false;
  if (pillDot) {
    pillDot.className = 'jobs-pill-dot ' + (running.length > 0 ? 'running' : failed.length > 0 ? 'failed' : 'done');
  }
  if (pillText) {
    if (running.length > 0) {
      const j = running[0];
      const lp = j.lastProgress;
      const pct = (lp && lp.total > 0) ? Math.round((lp.current / lp.total) * 100) + '%' : '…';
      pillText.textContent = running.length + (running.length > 1 ? ' jobs' : ' job') + ' \xb7 ' + pct;
    } else if (failed.length > 0) {
      pillText.textContent = failed.length + ' failed';
    } else {
      const done = jobs.filter(function(j) { return j.status === 'done'; });
      pillText.textContent = done.length + ' done';
    }
  }
}

function updateJobsTabBadge() {
  const badge = document.getElementById('jobs-tab-badge');
  if (!badge) return;
  const running = Object.values(activeJobs).filter(function(j) {
    return j.status === 'running' || j.status === 'reconnecting';
  }).length;
  badge.hidden = running === 0;
  badge.textContent = running > 0 ? running : '';
}

/* ══════════════════════════════════════════════════════════════════
   END JOBS SECTION
   ══════════════════════════════════════════════════════════════════ */

/* ── Summary view ── */
async function showSummary(path) {
  switchTab('tree');
  const view = document.getElementById('summary-view');
  view.style.display = 'block';
  view.innerHTML = '<div class="summary-pending">Loading summary…</div>';

  try {
    const r = await fetch('/api/summary?path=' + encodeURIComponent(path));
    const d = await r.json();

    if (d.error === 'no summary' || d.pending) {
      view.innerHTML = renderNoPendingSummary(path);
      return;
    }
    if (d.error) {
      view.innerHTML = '<div class="summary-pending" style="color:var(--red)">' + escapeHtml(d.error) + '</div>';
      return;
    }

    view.innerHTML = renderSummary(d);

    view.querySelectorAll('.child-item[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    view.querySelectorAll('.crumb[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    const enqBtn = view.querySelector('#enqueue-btn');
    if (enqBtn) {
      enqBtn.addEventListener('click', async function() {
        enqBtn.disabled = true;
        enqBtn.textContent = 'Queued…';
        await fetch('/api/summarize?path=' + encodeURIComponent(path), { method: 'POST' });
        setTimeout(function() { showSummary(path); }, 2000);
      });
    }
  } catch(e) {
    view.innerHTML = '<div class="summary-pending" style="color:var(--red)">Error: ' + escapeHtml(e.message) + '</div>';
  }
}

function renderNoPendingSummary(path) {
  const name = path.split('/').pop() || path;
  return '<div class="summary-text">' +
    '<div style="color:var(--muted);margin-bottom:12px">No summary yet for <strong>' + escapeHtml(name) + '</strong></div>' +
    '<button class="enqueue-btn" id="enqueue-btn">Generate summary</button>' +
    '</div>';
}

function renderSummary(d) {
  const name = d.path.split('/').pop() || d.path;
  const icon = d.kind === 'dir' ? '📁' : '📄';

  let crumbHtml = '';
  if (d.crumbs && d.crumbs.length) {
    crumbHtml = '<div class="crumbs">' +
      d.crumbs.map(function(c) {
        return '<a class="crumb" data-path="' + escapeAttr(c.path) + '">' + escapeHtml(c.name) + '</a>';
      }).join('<span class="sep">›</span>') +
      '<span class="sep">›</span><span>' + escapeHtml(name) + '</span></div>';
  }

  let childrenHtml = '';
  if (d.children && d.children.length) {
    childrenHtml = '<div class="children-section"><h3>Contents (' + d.children.length + ')</h3>' +
      d.children.map(function(c) {
        const cIcon = c.kind === 'dir' ? '📁' : '📄';
        return '<div class="child-item" data-path="' + escapeAttr(c.path) + '">' +
          '<div class="child-row"><span>' + cIcon + '</span><span class="child-name">' + escapeHtml(c.name) + '</span></div>' +
          '<div class="child-summary">' + escapeHtml(c.summary) + '</div>' +
          '</div>';
      }).join('') + '</div>';
  }

  const ts = d.generated_at ? new Date(d.generated_at * 1000).toLocaleDateString() : '';
  return crumbHtml +
    '<div class="summary-header"><span style="font-size:22px">' + icon + '</span>' +
    '<span class="summary-title">' + escapeHtml(name) + '</span></div>' +
    '<div class="summary-meta">Model: ' + escapeHtml(d.model) + (ts ? ' \xb7 ' + ts : '') + '</div>' +
    '<div class="summary-text">' + escapeHtml(d.summary) + '</div>' +
    childrenHtml;
}

/* ── Chat / Ask ── */
const chat = document.getElementById('chat');
const qInput = document.getElementById('q');
const sendBtn = document.getElementById('send');

function appendMsg(role, html) {
  const welcome = chat.querySelector('.welcome');
  if (welcome) welcome.remove();
  const div = document.createElement('div');
  div.className = 'msg ' + role;
  div.innerHTML = '<div class="bubble">' + html + '</div>';
  chat.appendChild(div);
  chat.scrollTop = chat.scrollHeight;
  return div;
}

async function doAsk() {
  const q = qInput.value.trim();
  if (!q) return;
  qInput.value = '';
  sendBtn.disabled = true;
  switchTab('chat');

  appendMsg('user', escapeHtml(q));
  const thinking = appendMsg('assistant', '<span class="thinking">Thinking…</span>');

  try {
    const r = await fetch('/api/ask', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ question: q })
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || 'Request failed');

    let html = escapeHtml(d.answer);
    if (d.sources && d.sources.length > 0) {
      html += '<div class="sources"><h4>Sources</h4>' +
        d.sources.map(function(s) {
          return '<div class="source-item"><span class="path">' + escapeHtml(s.path) + '</span>' +
            (s.heading ? '<span class="heading">' + escapeHtml(s.heading) + '</span>' : '') +
            '<div class="snippet">' + escapeHtml(s.snippet) + '</div></div>';
        }).join('') + '</div>';
    }
    thinking.querySelector('.bubble').innerHTML = html;
  } catch(err) {
    thinking.querySelector('.bubble').innerHTML = '<span style="color:var(--red)">' + escapeHtml(err.message) + '</span>';
  }

  sendBtn.disabled = false;
  qInput.focus();
  chat.scrollTop = chat.scrollHeight;
}

sendBtn.addEventListener('click', doAsk);
qInput.addEventListener('keydown', function(e) { if (e.key === 'Enter') doAsk(); });

/* ── Settings ── */
let settingsLoaded = false;
async function loadSettings() {
  if (settingsLoaded) return;
  settingsLoaded = true;
  loadModels();
  loadKeys();
  loadPasses();
}
async function loadPasses() {
  try {
    const r = await fetch('/api/config');
    if (!r.ok) return;
    const d = await r.json();
    document.getElementById('passes-first').value = d.passes_first || 2;
    document.getElementById('passes-refresh').value = d.passes_refresh || 1;
  } catch(_) {}
}
async function savePasses() {
  const first = parseInt(document.getElementById('passes-first').value, 10);
  const refresh = parseInt(document.getElementById('passes-refresh').value, 10);
  const status = document.getElementById('passes-status');
  try {
    const r = await fetch('/api/config/passes', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({passes_first: first, passes_refresh: refresh})
    });
    const d = await r.json();
    if (d.error) { status.style.color = 'var(--red)'; status.textContent = d.error; return; }
    status.style.color = 'var(--green)';
    status.textContent = 'Saved';
    setTimeout(function() { status.textContent = ''; }, 3000);
  } catch(e) {
    status.style.color = 'var(--red)';
    status.textContent = 'Error: ' + e.message;
  }
}

/* ── Queue stats (shown in Jobs tab) ── */
async function pollQueue() {
  try {
    const r = await fetch('/api/queue');
    const d = await r.json();
    // Update the Jobs tab queue row (if visible)
    const queueEl = document.getElementById('jobs-queue-stats');
    if (!queueEl) return;
    const total = d.pending + d.in_flight + d.failed;
    if (total === 0) {
      queueEl.textContent = 'Summary queue: idle';
      queueEl.style.color = 'var(--muted)';
      return;
    }
    var parts = [];
    if (d.pending > 0) parts.push(d.pending.toLocaleString() + ' pending');
    if (d.in_flight > 0) parts.push(d.in_flight + ' running');
    if (d.failed > 0) parts.push(d.failed + ' failed');
    queueEl.textContent = 'Summary queue: ' + parts.join(' \xb7 ');
    queueEl.style.color = d.failed > 0 ? 'var(--red)' : d.in_flight > 0 ? 'var(--accent)' : 'var(--muted)';
  } catch(_) {}
}
setInterval(pollQueue, 5000);
pollQueue();

/* ── Map view ── */
let mapLoaded = false;
async function loadMap() {
  if (mapLoaded) return;
  mapLoaded = true;
  const table = document.getElementById('map-table');
  try {
    const r = await fetch('/api/map');
    const d = await r.json();
    if (!d.length) {
      table.innerHTML = '<tr><td style="color:var(--muted);padding:12px 10px">No data yet. Run <code>indexa deep &lt;path&gt;</code> first.</td></tr>';
      return;
    }
    table.innerHTML = '<thead><tr><th>Category</th><th>Files</th><th>Size</th></tr></thead>';
    const tbody = document.createElement('tbody');
    d.forEach(function(row) {
      const tr = document.createElement('tr');
      const sz = row.total_size > 0 ? (row.total_size > 1048576 ? (row.total_size/1048576).toFixed(1)+' MB' : (row.total_size/1024).toFixed(0)+' KB') : '';
      tr.innerHTML = '<td>' + escapeHtml(row.category || 'Unknown') + '</td><td style="text-align:right">' + (row.entry_count||0).toLocaleString() + '</td><td style="text-align:right">' + sz + '</td>';
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
  } catch(e) {
    table.innerHTML = '<tr><td style="color:var(--red)">' + escapeHtml(e.message) + '</td></tr>';
  }
}

async function loadModels() {
  const list = document.getElementById('models-list');
  try {
    const r = await fetch('/api/models/installed');
    const models = await r.json();
    if (models.error) throw new Error(models.error);
    if (!models.length) {
      list.innerHTML = '<div style="color:var(--muted);font-size:13px">No models installed. Pull one below.</div>';
      return;
    }
    list.innerHTML = models.map(function(m) {
      const mb = m.size > 0 ? (m.size / 1024 / 1024).toFixed(0) + ' MB' : '';
      return '<div class="model-row"><span class="model-name">' + escapeHtml(m.name) + '</span>' +
        '<span class="model-size">' + mb + '</span></div>';
    }).join('');
  } catch(e) {
    list.innerHTML = '<div style="color:var(--red);font-size:13px">Ollama not reachable: ' + escapeHtml(e.message) + '</div>';
  }
}

async function pullModel() {
  const input = document.getElementById('pull-input');
  const name = input.value.trim();
  if (!name) return;
  const btn = document.getElementById('pull-btn');
  const prog = document.getElementById('pull-progress');
  btn.disabled = true;
  prog.style.display = 'block';
  prog.textContent = 'Starting pull for ' + name + '…\n';
  try {
    const r = await fetch('/api/models/pull', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({name: name})
    });
    if (!r.ok) { const d = await r.json(); throw new Error(d.error || 'Failed'); }
    const reader = r.body.getReader();
    const dec = new TextDecoder();
    while (true) {
      const {done, value} = await reader.read();
      if (done) break;
      const lines = dec.decode(value, {stream: true}).split('\n').filter(Boolean);
      lines.forEach(function(line) {
        try {
          const d = JSON.parse(line);
          if (d.status) prog.textContent += d.status + (d.completed ? ' ' + d.completed : '') + '\n';
          prog.scrollTop = prog.scrollHeight;
        } catch(_) {}
      });
    }
    prog.textContent += '✓ Done.\n';
    input.value = '';
    settingsLoaded = false;
    setTimeout(loadModels, 500);
  } catch(e) {
    prog.textContent += '✗ Error: ' + e.message + '\n';
  }
  btn.disabled = false;
}

async function loadKeys() {
  try {
    const r = await fetch('/api/keys');
    if (r.status === 403) {
      document.getElementById('key-gate-notice').style.display = 'block';
      ['openai','anthropic','google'].forEach(function(p) {
        document.getElementById('key-' + p).disabled = true;
      });
      return;
    }
    const d = await r.json();
    document.getElementById('badge-openai').textContent = d.openai_set ? '✓ set' : '';
    document.getElementById('badge-anthropic').textContent = d.anthropic_set ? '✓ set' : '';
    document.getElementById('badge-google').textContent = d.google_set ? '✓ set' : '';
  } catch(_) {}
}

async function saveKey(provider) {
  const val = document.getElementById('key-' + provider).value.trim();
  if (!val) return clearKey(provider);
  const r = await fetch('/api/keys', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({provider: provider, key: val})
  });
  const d = await r.json();
  if (d.error) { toast(d.error, 'error'); return; }
  document.getElementById('key-' + provider).value = '';
  loadKeys();
}

async function clearKey(provider) {
  const r = await fetch('/api/keys', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({provider: provider, key: ''})
  });
  const d = await r.json();
  if (d.error) { toast(d.error, 'error'); return; }
  loadKeys();
}

/* ── Utilities ── */
function toast(msg, level) {
  level = level || 'info';
  const container = document.getElementById('toast');
  const el = document.createElement('div');
  el.className = 'toast-msg ' + level;
  el.innerHTML = escapeHtml(msg) + '<button class="toast-close" onclick="this.parentElement.remove()" title="Dismiss">\xd7</button>';
  container.appendChild(el);
  setTimeout(function() { if (el.parentElement) el.remove(); }, 4000);
}
function escapeHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}
function escapeAttr(s) { return escapeHtml(s); }

/* ── Sound ── */
let _audioCtx = null;
function playPing(kind) {
  if (localStorage.getItem('indexa_sound_muted') === '1') return;
  try {
    _audioCtx = _audioCtx || new (window.AudioContext || window.webkitAudioContext)();
    const ctx = _audioCtx;
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.connect(gain);
    gain.connect(ctx.destination);
    osc.type = 'sine';
    if (kind === 'ok') {
      osc.frequency.setValueAtTime(880, ctx.currentTime);
      osc.frequency.exponentialRampToValueAtTime(1320, ctx.currentTime + 0.12);
    } else {
      osc.frequency.setValueAtTime(280, ctx.currentTime);
      osc.frequency.exponentialRampToValueAtTime(140, ctx.currentTime + 0.20);
    }
    gain.gain.setValueAtTime(0.15, ctx.currentTime);
    gain.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.28);
    osc.start();
    osc.stop(ctx.currentTime + 0.28);
  } catch(_) {}
}
function toggleSound() {
  const muted = localStorage.getItem('indexa_sound_muted') === '1';
  localStorage.setItem('indexa_sound_muted', muted ? '0' : '1');
  document.getElementById('sound-toggle').innerHTML = muted ? '🔔' : '🔕';
}
(function initSoundToggle() {
  if (localStorage.getItem('indexa_sound_muted') === '1') {
    const btn = document.getElementById('sound-toggle');
    if (btn) btn.innerHTML = '🔕';
  }
})();

/* ── Version ── */
async function loadVersion() {
  try {
    const r = await fetch('/api/version');
    const d = await r.json();
    const el = document.getElementById('app-version');
    if (el && d.version) el.textContent = 'v' + d.version;
  } catch(_) {}
}

/* ── Re-index all ── */
async function reindexAll() {
  try {
    const r = await fetch('/api/roots');
    const roots = await r.json();
    if (!roots.length) { toast('No indexed roots yet.', 'warn'); return; }
    if (!confirm('Re-index ' + roots.length + ' root(s) with deep scan?')) return;
    for (const root of roots) { await fireJob('deep', root.path); }
  } catch(e) { toast('Failed: ' + e.message, 'error'); }
}

/* ── ⌘K Command palette ── */
const CMD_ACTIONS = [
  { icon: '💬', label: 'Switch to Ask', hint: '', action: function() { switchTab('chat'); qInput && qInput.focus(); } },
  { icon: '📁', label: 'Switch to Browse', hint: '', action: function() { switchTab('tree'); } },
  { icon: '🗺️', label: 'Switch to Map', hint: '', action: function() { switchTab('map'); } },
  { icon: '⚙️', label: 'Switch to Settings', hint: '', action: function() { switchTab('settings'); } },
  { icon: '⚙', label: 'Switch to Jobs', hint: '', action: function() { switchTab('jobs'); } },
  { icon: '🌙', label: 'Toggle theme', hint: '', action: toggleTheme },
  { icon: '+', label: 'Add root folder', hint: '', action: openAddRoot },
  { icon: '↻', label: 'Re-index all roots', hint: '', action: reindexAll },
];

var _cmdFocusedIdx = -1;

function openCmdPalette() {
  const dialog = document.getElementById('cmd-palette');
  if (!dialog) return;
  dialog.showModal();
  const input = document.getElementById('cmd-input');
  if (input) { input.value = ''; input.focus(); }
  renderCmdResults('');
}

function closeCmdPalette() {
  const dialog = document.getElementById('cmd-palette');
  if (dialog && dialog.open) dialog.close();
}

function renderCmdResults(query) {
  const container = document.getElementById('cmd-results');
  if (!container) return;
  const q = query.trim().toLowerCase();

  const matchedActions = CMD_ACTIONS.filter(function(a) {
    return !q || a.label.toLowerCase().indexOf(q) !== -1;
  });

  container.innerHTML = '';
  _cmdFocusedIdx = -1;

  if (!matchedActions.length && !q) {
    container.innerHTML = '<div class="cmd-empty">Type to search commands or folders…</div>';
    return;
  }
  if (!matchedActions.length) {
    container.innerHTML = '<div class="cmd-empty">No results for "' + escapeHtml(query) + '"</div>';
    return;
  }

  if (matchedActions.length) {
    const label = document.createElement('div');
    label.className = 'cmd-section-label';
    label.textContent = 'Commands';
    container.appendChild(label);
    matchedActions.forEach(function(a) {
      const el = document.createElement('div');
      el.className = 'cmd-item';
      el.setAttribute('role', 'option');
      el.innerHTML = '<span class="cmd-item-icon">' + a.icon + '</span><span class="cmd-item-label">' + escapeHtml(a.label) + '</span>';
      el.onclick = function() { closeCmdPalette(); a.action(); };
      container.appendChild(el);
    });
  }
}

function _cmdItems() {
  return Array.from(document.querySelectorAll('#cmd-results .cmd-item'));
}
function _cmdMoveFocus(dir) {
  const items = _cmdItems();
  if (!items.length) return;
  items.forEach(function(el) { el.classList.remove('focused'); });
  _cmdFocusedIdx = Math.max(0, Math.min(items.length - 1, _cmdFocusedIdx + dir));
  items[_cmdFocusedIdx].classList.add('focused');
  items[_cmdFocusedIdx].scrollIntoView({ block: 'nearest' });
}
function _cmdSelectFocused() {
  const items = _cmdItems();
  if (_cmdFocusedIdx >= 0 && items[_cmdFocusedIdx]) items[_cmdFocusedIdx].click();
}

(function initCmdPalette() {
  const input = document.getElementById('cmd-input');
  if (input) {
    input.addEventListener('input', function() { renderCmdResults(input.value); _cmdFocusedIdx = -1; });
    input.addEventListener('keydown', function(e) {
      if (e.key === 'ArrowDown') { e.preventDefault(); _cmdMoveFocus(1); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); _cmdMoveFocus(-1); }
      else if (e.key === 'Enter') { e.preventDefault(); _cmdSelectFocused(); }
      else if (e.key === 'Escape') { closeCmdPalette(); }
    });
  }
  const dialog = document.getElementById('cmd-palette');
  if (dialog) {
    dialog.addEventListener('click', function(e) {
      if (e.target === dialog) closeCmdPalette();
    });
  }
})();

/* ── Global keyboard shortcuts ── */
document.addEventListener('keydown', function(e) {
  if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
    e.preventDefault();
    openCmdPalette();
  }
  if (e.key === 'Escape') {
    closeCmdPalette();
  }
});

/* ── Add-root modal (kept for compatibility) ── */
function openAddRoot() {
  const modal = document.getElementById('add-root-modal');
  if (modal) modal.style.display = 'flex';
  browseFsTo(document.getElementById('add-root-path').value || '');
}
function closeAddRoot() {
  const modal = document.getElementById('add-root-modal');
  if (modal) modal.style.display = 'none';
}
function onRootPathInput(val) {
  clearTimeout(window._fsTimer);
  window._fsTimer = setTimeout(function() { browseFsTo(val); }, 300);
}
async function browseFsTo(path) {
  const browser = document.getElementById('fs-browser');
  if (!browser) return;
  browser.innerHTML = '<div class="fs-entry" style="color:var(--muted)">Loading…</div>';
  try {
    const r = await fetch('/api/fs/ls?path=' + encodeURIComponent(path || ''));
    const d = await r.json();
    if (d.error) { browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">' + escapeHtml(d.error) + '</div>'; return; }
    browser.innerHTML = '';
    if (d.parent) {
      const up = document.createElement('div');
      up.className = 'fs-entry';
      up.innerHTML = '↑ ..';
      up.onclick = function() {
        document.getElementById('add-root-path').value = d.parent;
        browseFsTo(d.parent);
      };
      browser.appendChild(up);
    }
    (d.entries || []).forEach(function(entry) {
      const el = document.createElement('div');
      el.className = 'fs-entry' + (entry.is_dir ? '' : ' fs-file');
      el.textContent = (entry.is_dir ? '📁 ' : '📄 ') + entry.name;
      if (entry.is_dir) {
        el.onclick = function() {
          document.getElementById('add-root-path').value = entry.path;
          browseFsTo(entry.path);
        };
      }
      browser.appendChild(el);
    });
  } catch(e) {
    browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">' + escapeHtml(e.message) + '</div>';
  }
}
async function startIndexRoot() {
  const path = document.getElementById('add-root-path').value.trim();
  if (!path) { toast('Enter a folder path', 'warn'); return; }
  closeAddRoot();
  try { await fireJob('index', path); }
  catch(e) { toast('Failed: ' + e.message, 'error'); }
}

/* ── Init ── */
loadStats();
loadVersion();
initTree();
switchTab('chat');
reconnectInFlightJobs();
