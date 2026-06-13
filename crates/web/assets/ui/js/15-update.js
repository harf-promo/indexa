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
            // Scroll the update section into view after the panel opens.
            setTimeout(function () {
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
              '“Indexa → Check for Updates…” to install it.'
            : '✓ You’re on the latest (v' + d.current + '). To check anytime, choose ' +
              '“Indexa → Check for Updates…” in the menu bar (or the Indexa tray icon).';
          status.style.color = d.update_available ? 'var(--accent)' : 'var(--muted)';
        }
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
