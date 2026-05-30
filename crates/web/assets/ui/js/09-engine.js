/* ── Engine status bar ───────────────────────────────────────────────────────
   Subscribes to /api/telemetry/stream (SSE) and renders the always-on bottom bar:
   CPU sparkline, RAM meter with the keep-free headroom band, and a memory-pressure
   pip. Live whether the engine is idle or building — independent of any job.
   Honest two-signal design: RAM-fit (budget/headroom) and swap-pressure are shown
   separately, so "elevated swap while RAM is free" reads as exactly that. */
(function () {
  var CPU_HISTORY = [];
  var CPU_HISTORY_MAX = 25;

  function el(id) { return document.getElementById(id); }

  function fmtGB(bytes) {
    var gb = bytes / 1073741824;
    return gb.toFixed(gb < 10 ? 1 : 0) + ' GB';
  }

  function clampPct(n) { return Math.max(0, Math.min(100, n || 0)); }

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
    var rv = el('engine-ram-val');
    if (rv && s.ram) rv.textContent = fmtGB(s.ram.used_bytes) + ' / ' + fmtGB(total);

    // Pressure pip + neutral, honest label
    var pip = el('engine-pressure-pip');
    if (pip) pip.className = 'pressure-pip p-' + pressure;
    var pv = el('engine-pressure-val');
    if (pv) {
      pv.textContent = pressure === 'critical' ? 'high swap'
        : (pressure === 'throttle' ? 'elevated swap' : 'no pressure');
    }

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

  function init() {
    if (el('engine-bar')) connect();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
