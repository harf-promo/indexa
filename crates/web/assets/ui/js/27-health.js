// ── Index health banner (v0.39) ───────────────────────────────────────────────
// On load, check /api/health; if the index is stale (newest content > a week old)
// show a dismissible banner so the user knows answers may miss recent changes —
// the visible counter to the silent index-rot that made stale context look fresh.
(function () {
  document.addEventListener('DOMContentLoaded', function () {
    fetch('/api/health')
      .then(function (r) { return r.json(); })
      .then(function (h) {
        if (!h || !h.stale || document.getElementById('health-banner')) return;
        var bar = document.createElement('div');
        bar.id = 'health-banner';
        bar.style.cssText =
          'position:sticky;top:0;z-index:200;display:flex;justify-content:space-between;' +
          'align-items:center;gap:12px;padding:8px 14px;background:var(--surface-2);' +
          'border-bottom:1px solid var(--border);color:var(--text);font-size:13px;';
        var age = h.index_age_days == null ? '' : ' (' + h.index_age_days + ' days old)';
        var msg = document.createElement('span');
        msg.textContent =
          '⚠ Your index is stale' + age +
          ' — answers may miss recent changes. Re-index, or turn on Watch to keep it current.';
        var x = document.createElement('button');
        x.textContent = '✕';
        x.title = 'Dismiss';
        x.setAttribute('aria-label', 'Dismiss staleness warning');
        x.style.cssText =
          'background:none;border:none;color:var(--muted);cursor:pointer;font-size:14px;';
        x.addEventListener('click', function () { bar.remove(); });
        bar.appendChild(msg);
        bar.appendChild(x);
        document.body.insertBefore(bar, document.body.firstChild);
      })
      .catch(function () { /* health is best-effort — never block the UI */ });
  });
})();
