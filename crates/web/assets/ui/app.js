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
  ['tree','chat','map','settings'].forEach(function(t) {
    const btn = document.getElementById('tab-' + t);
    if (btn) btn.classList.toggle('active', t === tab);
    const panel = document.getElementById('panel-' + t);
    if (panel) panel.classList.toggle('active', t === tab);
  });
  // Legacy: also handle inner views for backward compat
  const sv = document.getElementById('summary-view');
  if (sv) sv.style.display = (tab === 'tree' && selectedPath !== null) ? 'block' : '';
  if (tab === 'settings') loadSettings();
  if (tab === 'map') loadMap();
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
  subscribeJob(d.job_id, path);
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

  // Alt/Option-click scopes the search to this folder
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
  document.getElementById('search-clear').style.display = val ? '' : 'none';
  clearTimeout(_searchTimer);
  if (!val.trim()) { initTree(); return; }
  _searchTimer = setTimeout(function() { doSearch(val.trim()); }, 200);
}
function clearSearchInput() {
  document.getElementById('search-input').value = '';
  document.getElementById('search-clear').style.display = 'none';
  initTree();
}
async function doSearch(q) {
  const list = document.getElementById('tree-list');
  list.innerHTML = '<div style="padding:8px 12px;color:var(--muted);font-size:12px">Searching…</div>';
  try {
    const r = await fetch('/api/search?q=' + encodeURIComponent(q) + '&limit=50');
    const nodes = await r.json();
    if (!nodes.length) {
      list.innerHTML = '<div style="padding:8px 12px;color:var(--muted);font-size:12px">No results</div>';
      return;
    }
    list.innerHTML = '';
    nodes.forEach(function(node) { list.appendChild(buildTreeNode(node)); });
  } catch(e) {
    list.innerHTML = '<div style="padding:8px 12px;color:var(--red);font-size:12px">Search error</div>';
  }
}

/* ── Add-Root modal ── */
var _rootPathDebounce = null;
function openAddRoot() {
  document.getElementById('add-root-modal').classList.add('open');
  browseFsTo('');
}
function closeAddRoot() {
  document.getElementById('add-root-modal').classList.remove('open');
}
function onRootPathInput(val) {
  clearTimeout(_rootPathDebounce);
  _rootPathDebounce = setTimeout(function() { browseFsTo(val); }, 350);
}
async function browseFsTo(path) {
  if (path) document.getElementById('add-root-path').value = path;
  const browser = document.getElementById('fs-browser');
  browser.innerHTML = '<div class="fs-entry" style="color:var(--muted)">Loading…</div>';
  try {
    const r = await fetch('/api/fs/ls?path=' + encodeURIComponent(path || ''));
    if (!r.ok) {
      const d = await r.json().catch(function(){return {};});
      browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">' + escapeHtml(d.error || 'Permission denied') + '</div>';
      return;
    }
    const entries = await r.json();
    browser.innerHTML = '';
    if (path) {
      const up = document.createElement('div');
      up.className = 'fs-entry';
      up.style.color = 'var(--muted)';
      up.innerHTML = '⤴ ..';
      up.onclick = function() {
        const parts = path.replace(/\/$/, '').split('/');
        parts.pop();
        browseFsTo(parts.join('/') || '/');
      };
      browser.appendChild(up);
    }
    if (!entries.length) {
      const empty = document.createElement('div');
      empty.className = 'fs-entry';
      empty.style.color = 'var(--muted)';
      empty.textContent = 'No subdirectories';
      browser.appendChild(empty);
    } else {
      entries.forEach(function(e) {
        const el = document.createElement('div');
        el.className = 'fs-entry';
        el.innerHTML = '📁 ' + escapeHtml(e.name);
        el.onclick = function() { browseFsTo(e.path); };
        browser.appendChild(el);
      });
    }
  } catch(err) {
    browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">Error</div>';
  }
}
async function startIndexRoot() {
  const path = document.getElementById('add-root-path').value.trim();
  if (!path) { toast('Enter a path first.', 'warn'); return; }
  try {
    closeAddRoot();
    await fireJob('index', path);
  } catch(e) {
    toast('Failed to start indexing: ' + e.message, 'error');
  }
}

/* ── Jobs dock ── */
function toggleJobsDock() {
  const dock = document.getElementById('jobs-panel');
  if (!dock) return;
  dock.classList.toggle('collapsed');
  const toggle = document.getElementById('jobs-dock-toggle');
  if (toggle) toggle.textContent = dock.classList.contains('collapsed') ? '▴' : '▾';
}

var activeJobs = {};
var _pendingProgress = {};
var _pendingLlm = {};
var _rafPending = false;

function _drainProgress() {
  _rafPending = false;
  for (var jid in _pendingProgress) { _applyProgress(jid, _pendingProgress[jid]); }
  _pendingProgress = {};
  for (var jid in _pendingLlm) { _applyLlmOutput(jid, _pendingLlm[jid]); }
  _pendingLlm = {};
}

function _applyLlmOutput(jobId, pending) {
  var row = document.getElementById('job-row-' + jobId);
  if (!row) return;
  var pre = row.querySelector('.job-ai-pre');
  var label = row.querySelector('.job-ai-label');
  if (!pre) return;
  if (pending.label && label) label.textContent = pending.label;
  if (pending.reset) pre.textContent = '';
  pre.textContent += pending.text;
  if (pre.textContent.length > 4096) {
    pre.textContent = pre.textContent.slice(pre.textContent.length - 4096);
  }
  var panel = row.querySelector('.job-ai-panel');
  if (panel && panel.classList.contains('open')) {
    pre.scrollTop = pre.scrollHeight;
  }
}

function _applyProgress(jobId, ev) {
  var row = document.getElementById('job-row-' + jobId);
  if (!row) return;
  var bar = row.querySelector('.job-progress');
  var fileEl = row.querySelector('.job-file');
  var speedEl = row.querySelector('.job-speed');
  var llmEl = row.querySelector('.job-llm-note');
  var statusEl = row.querySelector('.job-status');
  if (bar && ev.total) { bar.max = ev.total; bar.value = ev.current; }
  if (fileEl) {
    if (ev.current_path) {
      var parts = ev.current_path.split('/');
      var short = parts.length > 2 ? '…/' + parts.slice(-2).join('/') : ev.current_path;
      fileEl.textContent = short;
      fileEl.title = ev.current_path;
    } else {
      fileEl.textContent = '';
      fileEl.title = '';
    }
  }
  if (speedEl) {
    var sp = [];
    if (ev.items_per_sec && ev.items_per_sec > 0) sp.push(Math.round(ev.items_per_sec) + ' files/s');
    if (ev.eta_secs && ev.eta_secs > 0) {
      var eta = ev.eta_secs < 60 ? Math.round(ev.eta_secs) + 's' : Math.round(ev.eta_secs / 60) + 'm';
      sp.push('ETA ' + eta);
    }
    speedEl.textContent = sp.join(' \xb7 ');
  }
  if (llmEl && ev.note) llmEl.textContent = ev.note;
  if (statusEl) statusEl.textContent = ev.current + '/' + ev.total;
}

function getOrCreateJobRow(jobId) {
  if (activeJobs[jobId]) return activeJobs[jobId].row;
  const dock = document.getElementById('jobs-panel');
  if (dock) dock.style.display = '';
  const list = document.getElementById('jobs-list');
  const row = document.createElement('div');
  row.className = 'job-row';
  row.id = 'job-row-' + jobId;
  row.innerHTML =
    '<div class="job-row-header">' +
      '<span class="job-kind">…</span>' +
      '<span class="job-label">Starting…</span>' +
      '<span class="job-status running">●</span>' +
      '<button class="job-ai-toggle" title="Toggle AI output">✨</button>' +
    '</div>' +
    '<progress class="job-progress" style="display:none"></progress>' +
    '<div class="job-detail">' +
      '<span class="job-file"></span>' +
      '<span class="job-llm-note"></span>' +
      '<span class="job-speed"></span>' +
    '</div>' +
    '<div class="job-ai-panel">' +
      '<div class="job-ai-label"></div>' +
      '<pre class="job-ai-pre"></pre>' +
    '</div>';
  row.querySelector('.job-ai-toggle').onclick = function() {
    row.querySelector('.job-ai-panel').classList.toggle('open');
  };
  list.appendChild(row);
  activeJobs[jobId] = { row: row, es: null, warnings: [], _retries: 0 };
  return row;
}

/* ── localStorage helpers (B.2) ── */
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

/* ── Subscribe to a job's SSE stream (B.1 reconnect backoff) ── */
function subscribeJob(jobId, path) {
  const row = getOrCreateJobRow(jobId);
  row.querySelector('.job-label').textContent = (path || '').split('/').pop() || path || jobId;
  _saveActiveJob(jobId, path);

  const es = new EventSource('/api/jobs/' + jobId + '/events');
  activeJobs[jobId].es = es;
  if (!activeJobs[jobId]._retries) activeJobs[jobId]._retries = 0;

  es.onmessage = function(e) {
    try {
      const ev = JSON.parse(e.data);
      const kindEl = row.querySelector('.job-kind');
      const statusEl = row.querySelector('.job-status');
      const bar = row.querySelector('.job-progress');

      if (ev.type === 'start') {
        kindEl.textContent = ev.kind;
        statusEl.className = 'job-status running';
        statusEl.textContent = ev.total ? '0/' + ev.total : '…';
        // Reset retry counter on successful connect
        if (activeJobs[jobId]) activeJobs[jobId]._retries = 0;
      } else if (ev.type === 'snapshot') {
        if (bar && ev.count > 0) { bar.max = ev.count; bar.value = 0; bar.style.display = ''; }
        row.querySelector('.job-file').textContent = ev.count > 0 ? 'Starting…' : 'No files to process';
      } else if (ev.type === 'progress') {
        _pendingProgress[jobId] = ev;
        if (!_rafPending) { _rafPending = true; requestAnimationFrame(_drainProgress); }
      } else if (ev.type === 'done') {
        statusEl.className = 'job-status done';
        var warnCount = (activeJobs[jobId] && activeJobs[jobId].warnings) ? activeJobs[jobId].warnings.length : 0;
        statusEl.textContent = '✓ ' + ev.summary + (warnCount ? ' ⚠ ' + warnCount : '');
        if (bar) bar.style.display = 'none';
        playPing('ok');
        es.close();
        _removeActiveJob(jobId);
        setTimeout(function() {
          row.remove();
          delete activeJobs[jobId];
          const list = document.getElementById('jobs-list');
          if (list && !list.children.length) {
            const dock = document.getElementById('jobs-panel');
            if (dock) dock.style.display = 'none';
          }
          initTree();
          loadStats();
        }, 5000);
      } else if (ev.type === 'failed') {
        statusEl.className = 'job-status failed';
        var stage = ev.stage ? '[' + ev.stage + '] ' : '';
        statusEl.textContent = '✗ ' + stage + ev.error.slice(0, 60);
        if (ev.chain && ev.chain.length) {
          row.querySelector('.job-file').textContent = ev.chain.slice(0, 2).join(' → ');
        }
        if (bar) bar.style.display = 'none';
        playPing('err');
        es.close();
        _removeActiveJob(jobId);
        if (activeJobs[jobId]) activeJobs[jobId].failedEvent = ev;
        // Copy-report button
        var copyBtn = document.createElement('button');
        copyBtn.className = 'job-dismiss';
        copyBtn.title = 'Copy error report';
        copyBtn.textContent = '📋';
        var capturedEv = ev;
        copyBtn.onclick = async function() {
          try {
            var r = await fetch('/api/logs/tail?lines=50');
            var d = await r.json();
            var chain = capturedEv.chain && capturedEv.chain.length
              ? '\nCaused by:\n' + capturedEv.chain.map(function(c,i){return (i+1)+'. '+c;}).join('\n')
              : '';
            var report = '**Indexa error report**\n' +
              '- Version: ' + (document.getElementById('app-version').textContent || '?') + '\n' +
              '- Stage: ' + (capturedEv.stage || '?') + '\n' +
              (capturedEv.item_path ? '- Item: ' + capturedEv.item_path + '\n' : '') +
              '- Error: ' + capturedEv.error + chain + '\n\n' +
              '**Logs (last 50 lines)**\n' + (d.lines || '(no log file found)');
            await navigator.clipboard.writeText(report);
            toast('Error report copied to clipboard', 'info');
          } catch(err) { toast('Copy failed: ' + err.message, 'error'); }
        };
        // Dismiss button
        var dismissBtn = document.createElement('button');
        dismissBtn.className = 'job-dismiss';
        dismissBtn.title = 'Dismiss';
        dismissBtn.textContent = '\xd7';
        dismissBtn.onclick = function() {
          row.remove();
          delete activeJobs[jobId];
          const list = document.getElementById('jobs-list');
          if (list && !list.children.length) {
            const dock = document.getElementById('jobs-panel');
            if (dock) dock.style.display = 'none';
          }
        };
        row.querySelector('.job-row-header').appendChild(copyBtn);
        row.querySelector('.job-row-header').appendChild(dismissBtn);
      } else if (ev.type === 'warning') {
        if (!activeJobs[jobId]) return;
        activeJobs[jobId].warnings.push(ev);
        var wc = activeJobs[jobId].warnings.length;
        var warnEl = row.querySelector('.job-warn-count');
        if (!warnEl) {
          warnEl = document.createElement('span');
          warnEl.className = 'job-warn-count';
          row.querySelector('.job-detail').appendChild(warnEl);
        }
        warnEl.textContent = '⚠ ' + wc + (wc === 1 ? ' warning' : ' warnings');
        warnEl.title = activeJobs[jobId].warnings.map(function(w) {
          return (w.item_path ? w.item_path.split('/').pop() + ': ' : '') + w.message;
        }).join('\n');
      } else if (ev.type === 'llm_fragment') {
        var job = activeJobs[jobId];
        if (!job) return;
        var reset = job.lastLlmPath !== ev.item_path;
        job.lastLlmPath = ev.item_path;
        var label = ev.model + ' \xb7 ' + ev.stage;
        if (!_pendingLlm[jobId]) {
          _pendingLlm[jobId] = { text: ev.fragment, label: label, reset: reset };
        } else {
          if (reset) { _pendingLlm[jobId].reset = true; _pendingLlm[jobId].label = label; }
          _pendingLlm[jobId].text += ev.fragment;
        }
        if (!_rafPending) { _rafPending = true; requestAnimationFrame(_drainProgress); }
      }
    } catch(_) {}
  };

  /* B.1 — Reconnect with exponential backoff ── */
  es.onerror = function() {
    if (!activeJobs[jobId]) return;
    const statusEl = row.querySelector('.job-status');
    const isFinished = statusEl && (statusEl.className.indexOf('done') !== -1 || statusEl.className.indexOf('failed') !== -1);
    if (isFinished) return;

    es.close();
    activeJobs[jobId].es = null;

    fetch('/api/jobs/' + jobId).then(function(r) {
      if (r.status === 404) {
        _removeActiveJob(jobId);
        var j = activeJobs[jobId];
        if (j && j.row && j.row.parentNode) j.row.parentNode.removeChild(j.row);
        delete activeJobs[jobId];
        return;
      }
      var retries = (activeJobs[jobId] && activeJobs[jobId]._retries) || 0;
      var delays = [250, 500, 1000, 2000, 4000];
      var delay = delays[Math.min(retries, delays.length - 1)];
      if (statusEl && statusEl.className.indexOf('failed') === -1) {
        statusEl.className = 'job-status running';
        statusEl.textContent = retries > 2 ? 'reconnecting (' + retries + ')…' : 'reconnecting…';
      }
      setTimeout(function() {
        if (!activeJobs[jobId]) return;
        activeJobs[jobId]._retries = retries + 1;
        subscribeJob(jobId, path);
      }, delay);
    }).catch(function() {});
  };
}

/* B.2 — Reconnect in-flight jobs on page load ── */
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
        if (!activeJobs[j.job_id]) subscribeJob(j.job_id, j.path);
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

/* ── Queue badge ── */
async function pollQueue() {
  try {
    const r = await fetch('/api/queue');
    const d = await r.json();
    const badge = document.getElementById('queue-badge');
    if (!badge) return;
    const total = d.pending + d.in_flight + d.failed;
    if (total === 0) { badge.style.display = 'none'; return; }
    badge.style.display = '';
    let parts = [];
    if (d.pending > 0) parts.push(d.pending + ' pending');
    if (d.in_flight > 0) parts.push(d.in_flight + ' running');
    if (d.failed > 0) parts.push(d.failed + ' failed');
    badge.textContent = parts.join(' \xb7 ');
  } catch(_) {}
}
setInterval(pollQueue, 3000);
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
    container.innerHTML = '<div class="cmd-empty">No results for “' + escapeHtml(query) + '”</div>';
    return;
  }

  if (matchedActions.length) {
    const label = document.createElement('div');
    label.className = 'cmd-section-label';
    label.textContent = 'Commands';
    container.appendChild(label);
    matchedActions.forEach(function(a, i) {
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

/* ── Init ── */
loadStats();
loadVersion();
initTree();
switchTab('chat');
reconnectInFlightJobs();
