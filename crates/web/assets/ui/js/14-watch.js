/* ── Watch status (per-root eye icon) ──
   Tracks which roots are actively being watched by the embedded watcher.
   Refreshed every 10 s and immediately after start/stop. */

var watchedPaths = new Set();
var watchedInfo = {}; // path → { events_count, started_at } from /api/watch/status, when provided

async function refreshWatchStatus() {
  try {
    var r = await fetch('/api/watch/status');
    if (!r.ok) return;
    var list = await r.json();
    watchedPaths = new Set(list.map(function(e) { return e.path; }));
    watchedInfo = {};
    list.forEach(function(e) { watchedInfo[e.path] = e; });
    updateWatchIcons();
  } catch(_) {}
}
setInterval(refreshWatchStatus, 10000);
refreshWatchStatus();

// Turn a since-epoch-seconds start time into a short "for 5m" / "for 2h" suffix.
function watchAgeSuffix(startedAt) {
  if (!startedAt) return '';
  var secs = Math.max(0, Math.floor(Date.now() / 1000) - startedAt);
  if (secs < 90) return ' · watching for ' + secs + 's';
  if (secs < 5400) return ' · watching for ' + Math.round(secs / 60) + 'm';
  return ' · watching for ' + Math.round(secs / 3600) + 'h';
}

function updateWatchIcons() {
  document.querySelectorAll('.watch-toggle-btn').forEach(function(btn) {
    var path = btn.dataset.watchPath;
    var on = watchedPaths.has(path);
    btn.innerHTML = on ? ICO_EYE : ICO_EYE_OFF;
    // When the server reports live activity, surface it in the tooltip so the eye isn't just a
    // binary on/off — the user sees how many changes were re-indexed and how long it's been up.
    var extra = '';
    if (on) {
      var info = watchedInfo[path];
      if (info) {
        if (typeof info.events_count === 'number') {
          extra += ' · ' + info.events_count + ' change' + (info.events_count === 1 ? '' : 's') + ' re-indexed';
        }
        extra += watchAgeSuffix(info.started_at);
      }
    }
    btn.title = on ? ('Stop watching for changes' + extra) : 'Watch for changes (live re-embed on save)';
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
