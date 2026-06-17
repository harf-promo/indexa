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

      // ── "What's new" — cumulative changelog (installed → latest) ───────────
      // `d.notes` is the span of every version's notes between the running version
      // and the latest, assembled server-side (null when up to date or on a fetch
      // hiccup). Render it the same way the desktop update modal does.
      var whatsNew = document.getElementById('update-whats-new');
      if (whatsNew) {
        var raw = (d.update_available && d.notes) ? String(d.notes).trim() : '';
        if (raw) {
          whatsNew.innerHTML =
            '<details open><summary>What’s new</summary><div class="ucl-notes"></div></details>';
          var notesBody = whatsNew.querySelector('.ucl-notes');
          if (typeof renderMarkdown === 'function') {
            notesBody.innerHTML = renderMarkdown(
              typeof reflowChangelog === 'function' ? reflowChangelog(raw) : raw
            );
          } else {
            notesBody.textContent = raw;
          }
          whatsNew.style.display = '';
        } else {
          whatsNew.style.display = 'none';
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
  if (!p) return;
  // Phase "available" (desktop self-update) → show the in-app changelog window, not the progress
  // overlay. The user picks Install/Later there; the progress overlay takes over afterward.
  if (p.phase === 'available') { showUpdateChangelog(p); return; }

  var ov = document.getElementById('update-overlay');
  if (!ov) return;
  var fill = document.getElementById('update-overlay-fill');
  var titleEl = document.getElementById('update-overlay-title');
  var pctEl = document.getElementById('update-overlay-pct');
  var phaseEl = document.getElementById('update-overlay-phase');
  var dismiss = document.getElementById('update-overlay-dismiss');
  var phase = p.phase;

  if (phase === 'downloading' || phase === 'installing') {
    updateOverlayActive = true;
    hideUpdateChangelog(); // the changelog window is done; the progress bar takes over
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

// ── In-app "update available" changelog window (desktop only) ────────────────
// Shown on SSE phase "available" with the full release notes. Replaces the old native dialog —
// a white card with a scrollable changelog + Install & Relaunch / Later. The buttons POST to
// /api/update/control, which wakes the desktop's install task (the webview has no Tauri IPC).

// Reflow hard-wrapped CHANGELOG text into logical paragraphs before passing to
// renderMarkdown. Our CHANGELOG is hard-wrapped at ~100 cols with 2-space
// continuation indents; renderMarkdown is line-based, so a wrapped bullet's
// second line would close the <ul> prematurely. This pre-pass merges a
// non-blank continuation line into the previous logical line unless it starts
// a new block (heading, list item, fenced code, blank line).
function reflowChangelog(text) {
  if (!text) return text;
  var lines = text.split('\n');
  var out = [];
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i];
    var isBlock = !line.trim() ||           // blank
      /^#{1,6}\s/.test(line) ||             // heading
      /^\s*[-*]\s/.test(line) ||            // list item
      /^```/.test(line);                    // fenced code
    if (!isBlock && out.length > 0) {
      // Continuation: append to previous line with a space.
      out[out.length - 1] = out[out.length - 1] + ' ' + line.trim();
    } else {
      out.push(line);
    }
  }
  return out.join('\n');
}

function showUpdateChangelog(p) {
  var modal = document.getElementById('update-changelog-modal');
  if (!modal) return;
  var verEl = document.getElementById('ucl-version');
  var notesEl = document.getElementById('ucl-notes');
  var installBtn = document.getElementById('ucl-install');
  if (verEl) verEl.textContent = p.version || (p.title || '').replace(/^Indexa\s*/, '');
  if (notesEl) {
    var raw = p.notes && p.notes.trim() ? p.notes.trim() : 'No release notes available.';
    // Render as markdown (headings, bold, lists) instead of raw pre-wrap text.
    // reflowChangelog merges hard-wrapped continuation lines first so the
    // line-based renderer doesn't close list items prematurely.
    if (typeof renderMarkdown === 'function') {
      notesEl.innerHTML = renderMarkdown(reflowChangelog(raw));
    } else {
      notesEl.textContent = raw; // fallback if renderer not loaded (should not happen)
    }
    notesEl.scrollTop = 0;
  }
  if (installBtn) { installBtn.disabled = false; installBtn.textContent = 'Install & Relaunch'; }
  modal.hidden = false;
}

function hideUpdateChangelog() {
  var modal = document.getElementById('update-changelog-modal');
  if (modal) modal.hidden = true;
}

function sendUpdateControl(action) {
  return fetch('/api/update/control', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ action: action }),
  });
}

function dismissUpdateChangelog() {  // eslint-disable-line no-unused-vars
  hideUpdateChangelog();
  sendUpdateControl('dismiss').catch(function () { /* best-effort */ });
}

function installUpdateFromChangelog() {  // eslint-disable-line no-unused-vars
  var btn = document.getElementById('ucl-install');
  if (btn) { btn.disabled = true; btn.textContent = 'Downloading…'; }
  sendUpdateControl('start')
    .then(function () {
      // The progress overlay takes over when the next SSE event ("downloading") arrives;
      // renderUpdateProgress hides this modal then too.
      hideUpdateChangelog();
    })
    .catch(function () {
      if (btn) { btn.disabled = false; btn.textContent = 'Install & Relaunch'; }
    });
}
