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
        // A composite 'index' job runs scan→deep→summarize phases, each emitting
        // its own start. Keep the umbrella 'index' kind on the badge and surface
        // the current sub-phase separately instead of flipping the kind around.
        if (j.kind === 'index' && ev.kind && ev.kind !== 'index') {
          j.phase = ev.kind;
        } else {
          j.kind = ev.kind || j.kind || '?';
        }
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
        // Refresh tree (preserving expand/scroll state) and stats in background.
        setTimeout(function() { refreshTree(); loadStats(); }, 500);

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
      chipClass += ' running';
      chipText = j.status === 'reconnecting' ? 'reconnecting' : 'running';
      // For a composite index job, show which phase is currently running.
      if (j.phase) chipText += ' \xb7 ' + j.phase;
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
    // Memory-pressure warnings carry a structured snapshot (level + budget); show it as
    // a compact chip so the user can correlate with the Engine bar's RAM gauge.
    let pressureChip = '';
    if (w.pressure) {
      const p = w.pressure;
      const budgetMb = Math.round((p.budget_bytes || 0) / 1048576);
      const sign = budgetMb >= 0 ? '+' : '';
      pressureChip = '<span class="warn-pressure warn-pressure-' + escapeAttr(p.level) +
        '" title="budget ' + sign + budgetMb + ' MB · swap ' + p.swap_percent + '%">' +
        escapeHtml(p.level) + ' · budget ' + sign + budgetMb + ' MB</span>';
    }
    return '<div class="warn-row' + (w.pressure ? ' warn-row-pressure' : '') + '" onclick="this.classList.toggle(\'expanded\')">' +
      '<span class="warn-stage">' + escapeHtml(w.stage) + '</span>' +
      (basename ? '<span class="warn-path" title="' + escapeAttr(w.item_path) + '">' + escapeHtml(basename) + '</span>' : '') +
      '<span class="warn-msg">' + escapeHtml(w.message) + pressureChip + '</span>' +
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

