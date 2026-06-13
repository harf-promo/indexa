/* ── Engine status bar ───────────────────────────────────────────────────────
   Subscribes to /api/telemetry/stream (SSE) and renders the always-on bottom bar:
   CPU sparkline, RAM meter with the keep-free headroom band, and a memory-pressure
   pip. Live whether the engine is idle or building — independent of any job.
   Honest readout: RAM shows used + how much is actually free for a new model
   (the engine's budget), and the pressure pip reflects that same budget — not
   swap. A "Free models" button unloads Indexa's own resident models on demand. */
(function () {
  var CPU_HISTORY = [];
  var CPU_HISTORY_MAX = 25;

  function el(id) { return document.getElementById(id); }

  function fmtGB(bytes) {
    var gb = bytes / 1073741824;
    return gb.toFixed(gb < 10 ? 1 : 0) + ' GB';
  }

  function clampPct(n) { return Math.max(0, Math.min(100, n || 0)); }

  function fmtEta(secs) {
    if (!secs || secs <= 0) return '';
    return secs < 60 ? Math.round(secs) + 's' : Math.round(secs / 60) + 'm';
  }

  // Live build readout: the telemetry frame only carries the active job's identity
  // (id/kind/path); the per-file progress lives in the job-events SSE the UI already
  // opens (activeJobs[id].lastProgress / .llm). Fuse them here — no extra request,
  // no backend change. Degrades gracefully if the job stream hasn't delivered yet.
  function renderJob(s) {
    var box = el('engine-job');
    if (!box) return;
    var aj = s.active_job;
    // `activeJobs` is the global job store declared in 03-jobs-search.js (concatenated
    // before this file). Guard in case the ordering ever changes.
    var job = (aj && typeof activeJobs !== 'undefined') ? activeJobs[aj.job_id] : null;
    var lp = job && job.lastProgress;
    if (!aj || !lp || !(lp.total > 0)) {
      box.hidden = true;
      return;
    }
    box.hidden = false;

    var countEl = el('engine-job-count');
    if (countEl) countEl.textContent = lp.current + '/' + lp.total;

    var fill = el('engine-job-fill');
    if (fill) fill.style.width = clampPct(lp.current / lp.total * 100) + '%';

    var rateEl = el('engine-job-rate');
    if (rateEl) {
      var parts = [];
      if (lp.items_per_sec && lp.items_per_sec > 0) parts.push(lp.items_per_sec.toFixed(1) + '/s');
      var eta = fmtEta(lp.eta_secs);
      if (eta) parts.push('ETA ' + eta);
      rateEl.textContent = parts.join(' \xb7 ');
    }

    var fileEl = el('engine-job-file');
    if (fileEl) {
      var cp = lp.current_path || '';
      var short = cp ? cp.split('/').slice(-1)[0] : '';
      fileEl.textContent = short;
      fileEl.title = cp;
    }

    var modelEl = el('engine-job-model');
    if (modelEl) {
      // job.llm.label is "model · stage" while tokens stream; show just the model.
      var label = (job.llm && job.llm.label) ? job.llm.label.split(' \xb7 ')[0] : '';
      modelEl.textContent = label;
    }
  }

  function renderSpark(values) {
    var spark = el('engine-cpu-spark');
    if (!spark) return;
    var html = '';
    for (var i = 0; i < values.length; i++) {
      html += '<i style="height:' + Math.max(2, clampPct(values[i])) + '%"></i>';
    }
    spark.innerHTML = html;
  }

  function render(s) {
    var bar = el('engine-bar');
    var building = !!s.active_job;
    var pressure = s.pressure || 'ok';

    // State word: a job under pressure is "Easing off"; pressure with no job is
    // surfaced on the pip, not the state word (nothing is actually easing off).
    var word = 'Idle', cls = 'idle';
    if (building) {
      cls = 'building';
      word = 'Building';
      if (pressure === 'throttle' || pressure === 'critical') { word = 'Easing off'; cls = pressure; }
    }
    var wordEl = el('engine-state-word');
    if (wordEl) wordEl.textContent = word;
    if (bar) bar.className = 'engine-bar state-' + cls;

    // CPU
    if (s.cpu && typeof s.cpu.global_percent === 'number') {
      var cpu = Math.round(s.cpu.global_percent);
      var cv = el('engine-cpu-val');
      if (cv) cv.textContent = cpu + '%';
      CPU_HISTORY.push(cpu);
      while (CPU_HISTORY.length > CPU_HISTORY_MAX) CPU_HISTORY.shift();
      renderSpark(CPU_HISTORY);
    }

    // RAM meter + keep-free headroom band
    var total = s.ram && s.ram.total_bytes ? s.ram.total_bytes : 1;
    var used = el('engine-ram-used');
    if (used) used.style.width = clampPct(s.ram && s.ram.used_percent) + '%';
    var band = el('engine-ram-band');
    if (band) band.style.width = clampPct((s.headroom_bytes / total) * 100) + '%';
    var meter = el('engine-ram-meter');
    if (meter) meter.classList.toggle('in-band', !!s.in_headroom_band);
    // Honest value: show how much RAM is actually free for a NEW model (the
    // budget the engine computes) — not just used/total, which on macOS reads as
    // "almost full" because the OS keeps reclaimable cache resident. `budget` can
    // go negative when used+headroom exceed total; clamp at 0.
    var freeForModels = Math.max(0, (s.budget_bytes || 0));
    var rv = el('engine-ram-val');
    if (rv && s.ram) {
      rv.textContent = fmtGB(s.ram.used_bytes) + ' used \xb7 ' + fmtGB(freeForModels) + ' free';
    }
    var ramMetric = el('engine-ram-metric');
    if (ramMetric && s.ram) {
      ramMetric.title = fmtGB(s.ram.used_bytes) + ' used of ' + fmtGB(total)
        + ' (excludes reclaimable cache) \xb7 ' + fmtGB(freeForModels)
        + ' free for a new model above the ' + fmtGB(s.headroom_bytes || 0) + ' keep-free band';
    }

    // Pressure pip + honest, budget-based label. Pressure is derived from the
    // memory BUDGET (room above the keep-free headroom), not swap — so the label
    // must not say "swap" (it used to, misleadingly).
    var pip = el('engine-pressure-pip');
    if (pip) pip.className = 'pressure-pip p-' + pressure;
    var pv = el('engine-pressure-val');
    if (pv) {
      pv.textContent = pressure === 'critical' ? 'memory low'
        : (pressure === 'throttle' ? 'memory tight' : 'memory ok');
    }

    // Live build progress (fused from the per-job SSE the UI already holds).
    renderJob(s);

    // Machine summary
    var m = el('engine-machine');
    if (m && s.machine) {
      m.textContent = s.machine.logical_cores + ' cores · ' + fmtGB(s.machine.total_ram_bytes);
    }
  }

  function connect() {
    try {
      var es = new EventSource('/api/telemetry/stream');
      es.onmessage = function (ev) {
        try { render(JSON.parse(ev.data)); } catch (e) { /* ignore malformed frame */ }
      };
      // EventSource reconnects automatically on transient errors; nothing to do.
    } catch (e) { /* SSE unsupported — bar stays at its default placeholders */ }
  }

  // "Free models" button → unload Indexa's own resident local models. Exposed on
  // window because this module is an IIFE and the button uses an inline onclick.
  // This is NOT a system RAM purge — it only releases the models Indexa loaded;
  // the engine bar's streamed `used`/`free` updates a moment later as Ollama evicts.
  window.releaseModels = function () {
    var btn = el('engine-release-btn');
    if (btn) { btn.disabled = true; btn.classList.add('busy'); }
    fetch('/api/engine/release', { method: 'POST' })
      .then(function (r) { return r.json().catch(function () { return {}; }); })
      .then(function () {
        toast("Released Indexa's loaded models — memory frees as Ollama evicts them.", 'info');
      })
      .catch(function (e) { toast('Could not release models: ' + e.message, 'error'); })
      .finally(function () { if (btn) { btn.disabled = false; btn.classList.remove('busy'); } });
  };

  function init() {
    if (el('engine-bar')) connect();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
