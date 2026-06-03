/* ── Watch status (per-root eye icon) ──
   Tracks which roots are actively being watched by the embedded watcher.
   Refreshed every 10 s and immediately after start/stop. */

var watchedPaths = new Set();

async function refreshWatchStatus() {
  try {
    var r = await fetch('/api/watch/status');
    if (!r.ok) return;
    var list = await r.json();
    watchedPaths = new Set(list.map(function(e) { return e.path; }));
    updateWatchIcons();
  } catch(_) {}
}
setInterval(refreshWatchStatus, 10000);
refreshWatchStatus();

function updateWatchIcons() {
  document.querySelectorAll('.watch-toggle-btn').forEach(function(btn) {
    var path = btn.dataset.watchPath;
    var on = watchedPaths.has(path);
    btn.textContent = on ? '👁' : '👁‍🗨';
    btn.title = on ? 'Stop watching for changes' : 'Watch for changes (live re-embed on save)';
    btn.setAttribute('aria-label', on ? 'Stop watching' : 'Start watching');
    btn.classList.toggle('watch-on', on);
  });
}

async function toggleWatch(path) {
  var on = watchedPaths.has(path);
  try {
    var url = '/api/watch/' + (on ? 'stop' : 'start') + '?path=' + encodeURIComponent(path);
    var r = await fetch(url, { method: 'POST' });
    var d = await r.json();
    if (d.error) { toast(d.error, 'error'); return; }
    if (!on && d.watching) {
      watchedPaths.add(path);
      toast('Watching "' + escapeHtml(path.split('/').pop() || path) + '" — files re-indexed on save', 'info');
    } else if (on && d.stopped) {
      watchedPaths.delete(path);
      toast('Stopped watching "' + escapeHtml(path.split('/').pop() || path) + '"', 'info');
    }
    updateWatchIcons();
  } catch(e) { toast('Watch error: ' + e.message, 'error'); }
}
