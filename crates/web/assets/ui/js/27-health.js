// ── Index health banner (v0.39) + CLI-skew banner (v0.65) ─────────────────────
// On load, check /api/health and show dismissible banners for the two things that
// silently rotted before: a STALE index (newest content > a week old → answers may
// miss recent changes) and a STALE CLI/MCP binary (the desktop app updated but the
// terminal `indexa` it spawns didn't — surfaced via a marker the desktop writes).
(function () {
  // Build a sticky dismissible bar with a leading message span. Returns the bar
  // (not yet inserted) so the caller can append extra controls before the ✕.
  function makeBanner(id, messageText) {
    var bar = document.createElement('div');
    bar.id = id;
    bar.style.cssText =
      'position:sticky;top:0;z-index:200;display:flex;justify-content:space-between;' +
      'align-items:center;gap:12px;padding:8px 14px;background:var(--surface-2);' +
      'border-bottom:1px solid var(--border);color:var(--text);font-size:13px;';
    var msg = document.createElement('span');
    msg.textContent = messageText;
    bar.appendChild(msg);
    return bar;
  }

  function dismissButton(bar, label) {
    var x = document.createElement('button');
    x.textContent = '✕';
    x.title = 'Dismiss';
    x.setAttribute('aria-label', label);
    x.style.cssText =
      'background:none;border:none;color:var(--muted);cursor:pointer;font-size:14px;';
    x.addEventListener('click', function () { bar.remove(); });
    return x;
  }

  function showStaleIndexBanner(h) {
    if (!h.stale || document.getElementById('health-banner')) return;
    var age = h.index_age_days == null ? '' : ' (' + h.index_age_days + ' days old)';
    var bar = makeBanner(
      'health-banner',
      '⚠ Your index is stale' + age +
      ' — answers may miss recent changes. Re-index, or turn on Watch to keep it current.'
    );
    var reindexBtn = document.createElement('button');
    reindexBtn.textContent = 'Re-index now';
    reindexBtn.className = 'btn-sm';
    reindexBtn.title = 'Rebuild context for all roots';
    reindexBtn.setAttribute('aria-label', 'Re-index all roots now');
    reindexBtn.addEventListener('click', function () {
      bar.remove();
      if (typeof reindexAll === 'function') reindexAll();
    });
    bar.appendChild(reindexBtn);
    bar.appendChild(dismissButton(bar, 'Dismiss staleness warning'));
    document.body.insertBefore(bar, document.body.firstChild);
  }

  function showCliSkewBanner(h) {
    var s = h.cli_skew;
    if (!s || document.getElementById('cli-skew-banner')) return;
    var cli = s.cli_version ? 'v' + s.cli_version : 'an older version';
    var app = s.app_version ? 'v' + s.app_version : 'the new version';
    var bar = makeBanner(
      'cli-skew-banner',
      '⚠ Your terminal/MCP `indexa` is stale (' + cli + ') while the Indexa app is ' + app +
      ' — open a terminal and run `indexa update`, or reinstall the CLI from the app menu ' +
      '("Install command-line tool"). Restart the MCP server afterwards.'
    );
    bar.appendChild(dismissButton(bar, 'Dismiss CLI version warning'));
    document.body.insertBefore(bar, document.body.firstChild);
  }

  // Dismissal keys on the BUILT-DIRS count, not a plain boolean: re-dismissing after every
  // reload with unchanged coverage was worse than no dismiss button, but a flat "never show
  // again" would hide a real regression (or the banner never confirming real progress). If
  // the count when the user dismissed differs from the current count, coverage moved since
  // then — the banner is legitimately showing new information and returns. (Renamed from
  // "…at_summaries" — this used to key on the blended file+dir summary count; it now keys
  // on the real directory-coverage count `showThinContextBanner` actually displays, so a
  // stale value from before that fix simply won't match and the banner re-evaluates once.)
  var THIN_DISMISS_KEY = 'indexa_thin_dismissed_at_built_dirs';

  // `m` is the `/api/map` response — the same `total_dirs`/`built` figures the Map →
  // Table view shows as "N of M folders" — or `null` if that fetch failed/errored.
  // `h.thin_context` (from `/api/health`) is already computed from these same counts
  // server-side, so the banner still shows (it's the accurate trigger) even without
  // `m`; only the percentage in its text is omitted when the real numbers aren't in.
  function showThinContextBanner(h, m) {
    if (!h.thin_context || document.getElementById('thin-context-banner')) return;
    var haveCounts = !!m && typeof m.total_dirs === 'number' && typeof m.built === 'number';
    var totalDirs = haveCounts ? m.total_dirs : 0;
    var built = haveCounts ? m.built : 0;
    var dismissedAt = localStorage.getItem(THIN_DISMISS_KEY);
    if (haveCounts && dismissedAt !== null && Number(dismissedAt) === built) return;
    var pctText = haveCounts && totalDirs > 0
      ? ' (' + Math.round((100 * built) / totalDirs) + '% of folders summarized)'
      : '';
    var bar = makeBanner(
      'thin-context-banner',
      'Hierarchical context is thin' + pctText + ' — folder overviews and Export need summaries. Pick a project and click Build context.'
    );
    var dismiss = dismissButton(bar, 'Dismiss thin-context warning');
    dismiss.addEventListener('click', function () {
      if (haveCounts) localStorage.setItem(THIN_DISMISS_KEY, String(built));
    });
    bar.appendChild(dismiss);
    document.body.insertBefore(bar, document.body.firstChild);
  }

  document.addEventListener('DOMContentLoaded', function () {
    fetch('/api/health')
      .then(function (r) { return r.json(); })
      .then(function (h) {
        if (!h) return;
        showStaleIndexBanner(h);
        showCliSkewBanner(h);
        // Separate, best-effort fetch: if `/api/map` fails or errors, the thin-context
        // banner still renders off `h.thin_context` alone (see showThinContextBanner) —
        // the stale/CLI-skew banners above never depended on it either way. `r.ok` is
        // checked explicitly because a server error still returns valid JSON (`{"error":
        // …}` from `err_json`), which `.json()` would otherwise resolve as if it were
        // real coverage data.
        fetch('/api/map')
          .then(function (r) { return r.ok ? r.json() : null; })
          .then(function (m) { showThinContextBanner(h, m); })
          .catch(function () { showThinContextBanner(h, null); });
      })
      .catch(function () { /* health is best-effort — never block the UI */ });
  });
})();
