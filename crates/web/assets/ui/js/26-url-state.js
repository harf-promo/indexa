// ── Deep-linkable URL state (v0.37) ───────────────────────────────────────────
// Mirrors the active workspace tab + selected path + Ask question into location.hash
// so a view is bookmarkable/shareable, and restores it on load. Concatenated LAST,
// so the globals it reads (currentView/selectedPath/lastAskQuestion/askScope from
// 01 + 06) and the functions it calls (switchTab/showSummary/setAskScope) all exist
// at runtime. Local file paths in the hash only resolve on the owner's own machine.
//
// Restore fires exactly ONE /api/summary (via showSummary) and ZERO /api/ask — a
// restored question is typed into the box, never auto-run. __suppressHashWrite blocks
// the restore's own switchTab/showSummary from re-writing the hash (no loops).

'use strict';

var __suppressHashWrite = false;

// Parse "#tab=chat&path=...&q=...&scope=file" → {tab,path,q,scope} (values decoded).
function parseHash(hash) {
  var out = {};
  var h = String(hash || '').replace(/^#/, '');
  if (!h) return out;
  h.split('&').forEach(function (pair) {
    if (!pair) return;
    var i = pair.indexOf('=');
    var k = i < 0 ? pair : pair.slice(0, i);
    var v = i < 0 ? '' : pair.slice(i + 1);
    try { v = decodeURIComponent(v); } catch (e) { /* leave raw on malformed % */ }
    if (k === 'tab' || k === 'path' || k === 'q' || k === 'scope') out[k] = v;
  });
  return out;
}

// Build the hash from current state. 'tree' is the default view, so it's omitted to
// keep the common URL clean (#path=... alone implies the tree/Context view).
function serializeState() {
  var tab = (typeof currentView !== 'undefined') ? currentView : 'tree';
  var parts = [];
  if (tab === 'chat') {
    parts.push('tab=chat');
    if (typeof lastAskQuestion !== 'undefined' && lastAskQuestion) {
      parts.push('q=' + encodeURIComponent(lastAskQuestion));
    }
    if (typeof askScope !== 'undefined' && askScope) {
      parts.push('scope=file');
      parts.push('path=' + encodeURIComponent(askScope));
    }
  } else if (tab === 'map') {
    parts.push('tab=map');
  } else {
    if (typeof selectedPath !== 'undefined' && selectedPath) {
      parts.push('path=' + encodeURIComponent(selectedPath));
    }
  }
  return parts.join('&');
}

// Mirror current state into the URL. replaceState (not pushState) so tab clicks don't
// spam the back-stack — the URL just tracks state. eslint-disable-line no-unused-vars
function writeHash() {
  if (__suppressHashWrite) return;
  var next = serializeState();
  if (('#' + next) === location.hash) return;       // already current
  if (!next && !location.hash) return;               // nothing to clear
  var target = next ? '#' + next : location.pathname + location.search;
  try { history.replaceState(null, '', target); } catch (e) { location.hash = next; }
}

// Restore state from the hash on load / back-forward. Returns true if it took over the
// initial view (so boot skips its default switchTab('tree')). eslint-disable-line no-unused-vars
function restoreFromHash() {
  var st = parseHash(location.hash);
  if (!st.tab && !st.path && !st.q) return false;
  __suppressHashWrite = true;
  try {
    var tab = st.tab;
    if (tab !== 'chat' && tab !== 'map' && tab !== 'tree') {
      tab = st.path ? 'tree' : (st.q ? 'chat' : 'tree'); // validate; never a drawer
    }
    if (tab === 'chat') {
      if (st.scope === 'file' && st.path && typeof setAskScope === 'function') {
        if (typeof selectedPath !== 'undefined') selectedPath = st.path;
        setAskScope(st.path);
      }
      switchTab('chat');
      var qi = document.getElementById('q');
      if (qi && st.q) qi.value = st.q;               // display only — never auto-fire doAsk
    } else if (tab === 'map') {
      switchTab('map');
    } else if (st.path && typeof showSummary === 'function') {
      if (typeof selectedPath !== 'undefined') selectedPath = st.path;
      showSummary(st.path);                          // calls switchTab('tree') + renders
    } else {
      switchTab('tree');
    }
    return true;
  } catch (e) {
    return false;
  } finally {
    __suppressHashWrite = false;
  }
}
window.__indexaRestoreHash = restoreFromHash;

// Browser Back/Forward + manual hash edits re-restore (replaceState doesn't fire this).
window.addEventListener('hashchange', function () {
  if (__suppressHashWrite) return;
  restoreFromHash();
});
