// ── Self-update: version badge + one-click apply ─────────────────────────────
//
// On DOMContentLoaded, populates #app-version with the running version and
// (if an update is available) appends a clickable "↑ v{latest}" badge that
// opens the settings panel to the Software Update section.
//
// The "Update now" button in the settings panel calls applyUpdate().

document.addEventListener('DOMContentLoaded', function () {
  fetch('/api/update/check')
    .then(function (r) { return r.json(); })
    .then(function (d) {
      // ── Topbar version slot ───────────────────────────────────────────────
      var el = document.getElementById('app-version');
      if (el) {
        el.textContent = 'v' + d.current;

        if (d.update_available && d.latest) {
          el.classList.add('update-available');
          el.title = 'Update available: v' + d.latest + ' — click to open Settings';
          el.style.cursor = 'pointer';
          el.onclick = function () {
            switchTab('settings');
            // Expand (it starts collapsed in the accordion) + scroll into view.
            setTimeout(function () {
              if (typeof expandSettingsSection === 'function') expandSettingsSection('section-update');
              var sec = document.getElementById('section-update');
              if (sec) sec.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
            }, 120);
          };
        }
      }

      // ── Settings section ──────────────────────────────────────────────────
      var curEl = document.getElementById('update-current-ver');
      if (curEl) curEl.textContent = d.current;

      var badge = document.getElementById('update-version-badge');
      if (badge) {
        if (d.update_available && d.latest) {
          badge.textContent = 'v' + d.latest + ' available';
          badge.style.color = 'var(--accent)';
        } else if (d.latest) {
          badge.textContent = 'v' + d.latest + ' (up to date)';
        } else {
          badge.textContent = d.error ? 'check failed' : 'up to date';
        }
      }

      // Inside the desktop app, the in-page "Update now" button is disabled: the
      // app updates itself via the Tauri native updater (which installs the
      // notarized .app bundle). A web binary self-replace would corrupt the
      // bundle. Hide the button and point the user at the menu-bar item.
      if (d.desktop) {
        var btn = document.getElementById('update-apply-btn');
        if (btn) btn.style.display = 'none';
        var status = document.getElementById('update-apply-status');
        if (status) {
          // The desktop installs whole-bundle updates through the Tauri native updater
          // (a web binary self-replace would corrupt the signed .app). Point the user at
          // the menu bar — both the app menu and the tray icon expose it.
          status.textContent = d.update_available
            ? '⬆ Update v' + d.latest + ' is ready. In the menu bar, choose ' +
              '“Indexa → Check for Updates…” to see what’s new and install it.'
            : '✓ You’re on the latest (v' + d.current + '). To check anytime, choose ' +
              '“Indexa → Check for Updates…” in the menu bar (or the Indexa tray icon).';
          status.style.color = d.update_available ? 'var(--accent)' : 'var(--muted)';
        }
        // Point the user at the menu item that installs/updates the standalone `indexa`
        // command-line tool so their terminal version matches the app (the in-page button
        // can't drive native installs from the remote webview).
        var cliHint = document.getElementById('update-cli-hint');
        if (cliHint) {
          cliHint.textContent =
            'Use the terminal? Choose “Indexa → Install command-line tool” in the menu bar to ' +
            'put a matching ‘indexa’ command on your PATH.';
        }

        // Live download progress for the app self-update AND the CLI install. The desktop
        // process publishes to this SSE; the webview can't receive Tauri events directly (it
        // loads a remote URL with no IPC), so progress is bridged through the embedded server.
        openUpdateProgressStream();
      }
    })
    .catch(function () {
      // Network/parse error — leave the version slot blank; never break the UI.
      var badge = document.getElementById('update-version-badge');
      if (badge) badge.textContent = 'check failed';
    });
});

// Called by the "Update now" button in the Software Update settings section.
function applyUpdate() {  // eslint-disable-line no-unused-vars
  var statusEl = document.getElementById('update-apply-status');
  var btnEl = document.getElementById('update-apply-btn');

  if (statusEl) { statusEl.textContent = 'Downloading…'; statusEl.style.color = 'var(--muted)'; }
  if (btnEl) { btnEl.disabled = true; }

  fetch('/api/update/apply', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({}),
  })
    .then(function (r) { return r.json(); })
    .then(function (d) {
      if (d.error) {
        if (statusEl) { statusEl.textContent = d.error; statusEl.style.color = 'var(--red)'; }
        if (btnEl) { btnEl.disabled = false; }
        return;
      }
      var msg = d.relaunch === 'desktop'
        ? 'Updated to v' + d.version + ' — relaunching…'
        : 'Updated to v' + d.version + ' — restart indexa to apply.';
      if (statusEl) { statusEl.textContent = msg; statusEl.style.color = 'var(--green)'; }
    })
    .catch(function (e) {
      if (statusEl) { statusEl.textContent = 'Error: ' + e.message; statusEl.style.color = 'var(--red)'; }
      if (btnEl) { btnEl.disabled = false; }
    });
}

// ── Live update / CLI-install progress overlay (desktop only) ────────────────
//
// The desktop downloads the app update (Tauri updater) or the CLI tool in its Rust process and
// publishes progress to /api/update/progress/stream — the webview can't receive Tauri events
// (remote URL, no IPC), so it reads the bar over SSE instead. `updateOverlayActive` guards against
// a replayed stale terminal value on (re)connect: terminal states only render once we've seen a
// live downloading/installing this session.
var updateOverlayActive = false;

function openUpdateProgressStream() {
  if (typeof EventSource === 'undefined') return;
  try {
    var es = new EventSource('/api/update/progress/stream');
    es.onmessage = function (ev) {
      var p;
      try { p = JSON.parse(ev.data); } catch (_) { return; }
      renderUpdateProgress(p);
    };
    // On a dropped connection the browser auto-reconnects and the watch stream resends the
    // latest value; no manual handling needed.
  } catch (_) { /* never break the page over the progress stream */ }
}

function fmtUpdateBytes(bytes) {
  if (typeof bytes !== 'number') return '';
  return (bytes / 1048576).toFixed(1) + ' MB';
}

// Exposed for the overlay's Dismiss button (error state) and for tests.
function renderUpdateProgress(p) {  // eslint-disable-line no-unused-vars
  var ov = document.getElementById('update-overlay');
  if (!ov || !p) return;
  var fill = document.getElementById('update-overlay-fill');
  var titleEl = document.getElementById('update-overlay-title');
  var pctEl = document.getElementById('update-overlay-pct');
  var phaseEl = document.getElementById('update-overlay-phase');
  var dismiss = document.getElementById('update-overlay-dismiss');
  var phase = p.phase;

  if (phase === 'downloading' || phase === 'installing') {
    updateOverlayActive = true;
    ov.hidden = false;
    if (dismiss) dismiss.hidden = true;
    if (phaseEl) phaseEl.classList.remove('update-overlay-error');
    if (titleEl) titleEl.textContent = p.title || 'Updating Indexa…';
  }

  if (phase === 'downloading') {
    if (p.total && p.total > 0) {
      var pct = Math.min(100, Math.round((p.downloaded / p.total) * 100));
      if (fill) { fill.classList.remove('indeterminate'); fill.style.width = pct + '%'; }
      if (pctEl) pctEl.textContent = pct + '% · ' + fmtUpdateBytes(p.downloaded) + ' / ' + fmtUpdateBytes(p.total);
    } else {
      if (fill) fill.classList.add('indeterminate');
      if (pctEl) pctEl.textContent = fmtUpdateBytes(p.downloaded) + ' downloaded…';
    }
    if (phaseEl) phaseEl.textContent = 'Downloading…';
  } else if (phase === 'installing') {
    if (fill) { fill.classList.remove('indeterminate'); fill.style.width = '100%'; }
    if (pctEl) pctEl.textContent = '';
    if (phaseEl) phaseEl.textContent = 'Installing…';
  } else if (phase === 'done') {
    if (!updateOverlayActive) return;
    if (fill) { fill.classList.remove('indeterminate'); fill.style.width = '100%'; }
    if (pctEl) pctEl.textContent = '';
    if (phaseEl) phaseEl.textContent = 'Done — Indexa will restart.';
  } else if (phase === 'error') {
    if (!updateOverlayActive) return; // ignore a stale error replayed on connect
    ov.hidden = false;
    if (fill) fill.classList.remove('indeterminate');
    if (pctEl) pctEl.textContent = '';
    if (phaseEl) { phaseEl.textContent = p.error || 'Update failed.'; phaseEl.classList.add('update-overlay-error'); }
    if (dismiss) dismiss.hidden = false;
  }
  // phase 'idle' → no-op (don't pop the overlay on the initial connect snapshot).
}

function dismissUpdateOverlay() {  // eslint-disable-line no-unused-vars
  var ov = document.getElementById('update-overlay');
  if (ov) ov.hidden = true;
  updateOverlayActive = false;
}
