/* ── Utilities ── */
function toast(msg, level, action) {
  level = level || 'info';
  const container = document.getElementById('toast');
  const el = document.createElement('div');
  el.className = 'toast-msg ' + level;
  el.appendChild(document.createTextNode(msg));
  if (action && action.label && typeof action.onClick === 'function') {
    const act = document.createElement('button');
    act.className = 'toast-action';
    act.type = 'button';
    act.textContent = action.label;
    act.addEventListener('click', function () { action.onClick(); el.remove(); });
    el.appendChild(act);
  }
  const close = document.createElement('button');
  close.className = 'toast-close';
  close.title = 'Dismiss';
  close.textContent = '×';
  close.addEventListener('click', function () { el.remove(); });
  el.appendChild(close);
  container.appendChild(el);
  setTimeout(function() { if (el.parentElement) el.remove(); }, 4000);
}

/* In-app confirmation modal — returns a Promise<bool>. Replaces native confirm(),
   which blocks the event loop and freezes headless/automated browser sessions. */
function confirmModal(message, confirmLabel) {
  return new Promise(function(resolve) {
    // Restored on close (any path — Escape, backdrop click, Cancel, Confirm, or Enter) so
    // keyboard/focus state returns to wherever the user was before the modal opened.
    const prevFocus = document.activeElement;
    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay confirm-overlay';
    overlay.style.display = 'flex';
    const safeMsg = escapeHtml(message).replace(/\n/g, '<br>');
    overlay.innerHTML =
      '<div class="modal confirm-modal" role="dialog" aria-modal="true">' +
        '<div class="confirm-msg">' + safeMsg + '</div>' +
        '<div class="modal-actions">' +
          '<button class="modal-btn" data-act="cancel">Cancel</button>' +
          '<button class="modal-btn primary" data-act="ok">' + escapeHtml(confirmLabel || 'Confirm') + '</button>' +
        '</div>' +
      '</div>';
    const cancelBtn = overlay.querySelector('[data-act="cancel"]');
    const okBtn = overlay.querySelector('[data-act="ok"]');
    function close(result) {
      if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
      document.removeEventListener('keydown', onKey);
      if (prevFocus && typeof prevFocus.focus === 'function') prevFocus.focus();
      resolve(result);
    }
    function onKey(e) {
      if (e.key === 'Escape') { close(false); return; }
      if (e.key !== 'Enter') return;
      // Enter activates whichever button actually has keyboard focus. A user who Tabbed to
      // Cancel and pressed Enter must get Cancel, not Confirm — this used to fire Confirm
      // unconditionally regardless of focus, a real data-loss bug on any confirmModal call
      // gating a destructive action. Default to Confirm only when focus is elsewhere in the
      // dialog (e.g. the message body), matching typical dialog conventions — including the
      // common case where nothing was Tabbed and the initial focus (below) is still on okBtn.
      close(document.activeElement !== cancelBtn);
    }
    cancelBtn.onclick = function() { close(false); };
    okBtn.onclick = function() { close(true); };
    overlay.addEventListener('click', function(e) { if (e.target === overlay) close(false); });
    document.addEventListener('keydown', onKey);
    document.body.appendChild(overlay);
    if (okBtn) okBtn.focus();
  });
}
// Escapes `'` too, so a value is safe in a single-quoted attribute, not only a double-quoted one.
function escapeHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;');
}
function escapeAttr(s) { return escapeHtml(s); }

/* Minimal, XSS-safe markdown renderer for LLM answers. HTML is escaped FIRST,
   then a small set of markdown constructs are turned into tags. Supports fenced
   code blocks, inline code, bold, italic, headings, unordered lists, and GFM
   pipe tables (a `| h | h |` header row followed by a `|---|---|` separator). */
function renderMarkdown(src) {
  const safeSrc = String(src || '').replace(/\0/g, '');
  if (!safeSrc) return '';
  // 1) Escape everything up front so no raw HTML from the model can execute.
  let text = escapeHtml(safeSrc);

  // 2) Pull fenced code blocks out into placeholders so inline rules don't touch them.
  const blocks = [];
  text = text.replace(/```([\s\S]*?)```/g, function(_m, code) {
    blocks.push(code.replace(/^\n/, ''));
    return '\0CODEBLOCK' + (blocks.length - 1) + '\0';
  });

  // 3) Line-level: tables, headings, and unordered lists.
  const lines = text.split('\n');
  let out = '';
  let inList = false;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    // Table: a header row immediately followed by a |---|---| separator. Consume the
    // contiguous body rows that follow. Differently-shaped pipe lines fall through.
    if (line.indexOf('|') !== -1 && i + 1 < lines.length && isTableSeparator(lines[i + 1])) {
      if (inList) { out += '</ul>'; inList = false; }
      const header = splitTableRow(line);
      let j = i + 2;
      let bodyRows = '';
      while (j < lines.length && lines[j].trim() !== '' && lines[j].indexOf('|') !== -1) {
        bodyRows += '<tr>' + splitTableRow(lines[j]).map(function(c) {
          return '<td>' + inlineMd(c) + '</td>';
        }).join('') + '</tr>';
        j++;
      }
      out += '<table class="chat-table"><thead><tr>' +
        header.map(function(c) { return '<th>' + inlineMd(c) + '</th>'; }).join('') +
        '</tr></thead><tbody>' + bodyRows + '</tbody></table>';
      i = j - 1; // skip the rows we consumed
      continue;
    }
    const h = line.match(/^(#{1,4})\s+(.*)$/);
    const li = line.match(/^\s*[-*]\s+(.*)$/);
    if (li) {
      if (!inList) { out += '<ul>'; inList = true; }
      out += '<li>' + inlineMd(li[1]) + '</li>';
      continue;
    }
    if (inList) { out += '</ul>'; inList = false; }
    if (h) {
      const lvl = h[1].length;
      out += '<h' + lvl + '>' + inlineMd(h[2]) + '</h' + lvl + '>';
    } else if (line.trim() === '') {
      out += '<br>';
    } else if (line.indexOf('\0CODEBLOCK') !== -1) {
      out += line; // placeholder, restored below
    } else {
      out += inlineMd(line) + '<br>';
    }
  }
  if (inList) out += '</ul>';

  // 4) Restore code blocks as <pre><code>.
  out = out.replace(/\0CODEBLOCK(\d+)\0/g, function(_m, i) {
    return '<pre class="chat-code">' + blocks[+i] + '</pre>';
  });
  return out;
}

/* Split one pipe-table row into trimmed cells, tolerating optional leading/trailing
   pipes (`| a | b |` and `a | b` both → ['a','b']). Operates on already-escaped text. */
function splitTableRow(line) {
  return line.trim().replace(/^\|/, '').replace(/\|$/, '').split('|')
    .map(function(c) { return c.trim(); });
}

/* A GFM table separator: a pipe row whose every cell is dashes with optional
   alignment colons, e.g. `|---|:--:|`. */
function isTableSeparator(line) {
  if (line.indexOf('|') === -1) return false;
  const cells = splitTableRow(line);
  return cells.length > 0 && cells.every(function(c) { return /^:?-{1,}:?$/.test(c); });
}

/* Inline markdown: `code`, **bold**, *italic*. Operates on already-escaped text. */
function inlineMd(s) {
  return s
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^*])\*([^*]+)\*([^*]|$)/g, '$1<em>$2</em>$3');
}

/* ── Sound ── */
let _audioCtx = null;
function playPing(kind) {
  if (localStorage.getItem('indexa_sound_muted') === '1') return;
  try {
    _audioCtx = _audioCtx || new (window.AudioContext || window.webkitAudioContext)();
    const ctx = _audioCtx;
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.connect(gain);
    gain.connect(ctx.destination);
    osc.type = 'sine';
    if (kind === 'ok') {
      osc.frequency.setValueAtTime(880, ctx.currentTime);
      osc.frequency.exponentialRampToValueAtTime(1320, ctx.currentTime + 0.12);
    } else {
      osc.frequency.setValueAtTime(280, ctx.currentTime);
      osc.frequency.exponentialRampToValueAtTime(140, ctx.currentTime + 0.20);
    }
    gain.gain.setValueAtTime(0.15, ctx.currentTime);
    gain.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.28);
    osc.start();
    osc.stop(ctx.currentTime + 0.28);
  } catch(_) {}
}
function toggleSound() {
  const muted = localStorage.getItem('indexa_sound_muted') === '1';
  const next = !muted; // will be muted after this toggle
  localStorage.setItem('indexa_sound_muted', next ? '1' : '0');
  const btn = document.getElementById('sound-toggle');
  if (btn) {
    btn.innerHTML = next ? ICO_BELL_OFF : ICO_BELL_ON;
    // aria-pressed reflects "sound is muted" (the toggle's own state), same sense as the
    // icon — previously unset entirely here, so mute state was never announced.
    btn.setAttribute('aria-pressed', next ? 'true' : 'false');
    btn.setAttribute('aria-label', next ? 'Unmute sound notifications' : 'Mute sound notifications');
  }
}
(function initSoundToggle() {
  const btn = document.getElementById('sound-toggle');
  if (!btn) return;
  const muted = localStorage.getItem('indexa_sound_muted') === '1';
  if (muted) btn.innerHTML = ICO_BELL_OFF;
  btn.setAttribute('aria-pressed', muted ? 'true' : 'false');
  btn.setAttribute('aria-label', muted ? 'Unmute sound notifications' : 'Mute sound notifications');
})();

/* ── Version ── */
async function loadVersion() {
  try {
    const r = await fetch('/api/version');
    const d = await r.json();
    const el = document.getElementById('app-version');
    if (el && d.version) el.textContent = 'v' + d.version;
  } catch(_) {}
}

/* ── Re-index all ── */
async function reindexAll() {
  const btn = document.querySelector('button[onclick="reindexAll()"]');
  if (btn) btn.disabled = true;
  try {
    const r = await fetch('/api/roots');
    // A server error must read as an error, not silently reinterpreted as "no roots yet" —
    // an unchecked !roots.length on a non-2xx response's error body did exactly that.
    if (!r.ok) { toast('Could not fetch roots', 'error'); return; }
    const roots = await r.json();
    if (!roots.length) { toast('No context roots yet.', 'warn'); return; }
    if (!(await confirmModal('Rebuild context for ' + roots.length + ' root(s)? (deep + summarize)', 'Rebuild'))) return;
    for (const root of roots) { await fireJob('index', root.path); }
  } catch(e) { toast('Failed: ' + e.message, 'error'); }
  finally { if (btn) btn.disabled = false; }
}

/* ── ⌘K Command palette ── */
const CMD_ACTIONS = [
  { icon: ICO_CHAT, label: 'Switch to Ask', hint: '', action: function() { switchTab('chat'); qInput && qInput.focus(); } },
  { icon: ICO_FOLDER, label: 'Switch to Context', hint: '', action: function() { switchTab('tree'); } },
  { icon: ICO_MAP, label: 'Switch to Map', hint: '', action: function() { switchTab('map'); } },
  { icon: ICO_GEAR, label: 'Open Settings', hint: '', action: function() { switchTab('settings'); } },
  { icon: ICO_CLIPBOARD, label: 'Open Activity', hint: '', action: function() { switchTab('jobs'); } },
  { icon: ICO_MOON, label: 'Toggle theme', hint: '', action: toggleTheme },
  { icon: ICO_PLUS, label: 'Add root folder', hint: '', action: openAddRoot },
  { icon: ICO_REFRESH, label: 'Rebuild context for all roots', hint: '', action: reindexAll },
  { icon: ICO_DOWNLOAD, label: 'Export context as XML', hint: 'for Claude, Cursor, Copilot', action: function() { doExport(selectedPath || '', 'xml'); } },
  { icon: ICO_DOWNLOAD, label: 'Export context as Markdown', hint: '', action: function() { doExport(selectedPath || '', 'md'); } },
  { icon: ICO_DOWNLOAD, label: 'Export context as JSON', hint: '', action: function() { doExport(selectedPath || '', 'json'); } },
];

var _cmdFocusedIdx = -1;

function openCmdPalette() {
  const dialog = document.getElementById('cmd-palette');
  if (!dialog) return;
  dialog.showModal();
  const input = document.getElementById('cmd-input');
  if (input) { input.value = ''; input.focus(); }
  renderCmdResults('');
}

function closeCmdPalette() {
  const dialog = document.getElementById('cmd-palette');
  if (dialog && dialog.open) dialog.close();
}

function renderCmdResults(query) {
  const container = document.getElementById('cmd-results');
  if (!container) return;
  const q = query.trim().toLowerCase();

  const matchedActions = CMD_ACTIONS.filter(function(a) {
    return !q || a.label.toLowerCase().indexOf(q) !== -1;
  });

  container.innerHTML = '';
  _cmdFocusedIdx = -1;

  if (!matchedActions.length && !q) {
    container.innerHTML = '<div class="cmd-empty">Type to search commands…</div>';
    return;
  }
  if (!matchedActions.length) {
    container.innerHTML = '<div class="cmd-empty">No results for "' + escapeHtml(query) + '"</div>';
    return;
  }

  if (matchedActions.length) {
    const label = document.createElement('div');
    label.className = 'cmd-section-label';
    label.textContent = 'Commands';
    container.appendChild(label);
    matchedActions.forEach(function(a) {
      const el = document.createElement('div');
      el.className = 'cmd-item';
      el.setAttribute('role', 'option');
      el.innerHTML = '<span class="cmd-item-icon">' + a.icon + '</span><span class="cmd-item-label">' + escapeHtml(a.label) + '</span>';
      el.onclick = function() { closeCmdPalette(); a.action(); };
      container.appendChild(el);
    });
  }
}

function _cmdItems() {
  return Array.from(document.querySelectorAll('#cmd-results .cmd-item'));
}
function _cmdMoveFocus(dir) {
  const items = _cmdItems();
  if (!items.length) return;
  items.forEach(function(el) { el.classList.remove('focused'); });
  _cmdFocusedIdx = Math.max(0, Math.min(items.length - 1, _cmdFocusedIdx + dir));
  items[_cmdFocusedIdx].classList.add('focused');
  items[_cmdFocusedIdx].scrollIntoView({ block: 'nearest' });
}
function _cmdSelectFocused() {
  const items = _cmdItems();
  if (_cmdFocusedIdx >= 0 && items[_cmdFocusedIdx]) items[_cmdFocusedIdx].click();
}

(function initCmdPalette() {
  const input = document.getElementById('cmd-input');
  if (input) {
    input.addEventListener('input', function() { renderCmdResults(input.value); _cmdFocusedIdx = -1; });
    input.addEventListener('keydown', function(e) {
      if (e.key === 'ArrowDown') { e.preventDefault(); _cmdMoveFocus(1); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); _cmdMoveFocus(-1); }
      else if (e.key === 'Enter') { e.preventDefault(); _cmdSelectFocused(); }
      else if (e.key === 'Escape') { closeCmdPalette(); }
    });
  }
  const dialog = document.getElementById('cmd-palette');
  if (dialog) {
    dialog.addEventListener('click', function(e) {
      if (e.target === dialog) closeCmdPalette();
    });
  }
})();

/* ── Global keyboard shortcuts ── */
document.addEventListener('keydown', function(e) {
  if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
    e.preventDefault();
    openCmdPalette();
  }
  if (e.key === 'Escape') {
    closeCmdPalette();
  }
});

/* ── Add-root modal (kept for compatibility) ── */
let _addRootOpener = null;
function openAddRoot() {
  const modal = document.getElementById('add-root-modal');
  if (!modal) return;
  // Remember the trigger so focus can return to it on close (parity with the drawers).
  _addRootOpener = document.activeElement;
  // Use the shared .open class pattern (CSS: .modal-overlay.open { display:flex })
  // instead of inline style — consistent with the rest of the overlay system.
  modal.classList.add('open');
  browseFsTo(document.getElementById('add-root-path').value || '');
  const input = document.getElementById('add-root-path');
  if (input) {
    input.focus();
    // Enter-to-submit: bind once on first open (guard keeps re-opens clean).
    if (!input._addRootEnterBound) {
      input._addRootEnterBound = true;
      input.addEventListener('keydown', function(e) {
        if (e.key === 'Enter') { e.preventDefault(); startIndexRoot(); }
      });
    }
  }
  // Focus trap: make background regions inert while the modal is open, exactly
  // as the settings/jobs drawers do (setBackgroundInert defined in 01-state-theme-tabs.js).
  if (typeof setBackgroundInert === 'function') setBackgroundInert(true);
}
function closeAddRoot() {
  const modal = document.getElementById('add-root-modal');
  if (modal) modal.classList.remove('open');
  // Only lift the background inert if no drawers are also open (mirrors closeDrawer).
  if (typeof setBackgroundInert === 'function' && typeof anyDrawerOpen === 'function' && !anyDrawerOpen()) {
    setBackgroundInert(false);
  }
  if (_addRootOpener && typeof _addRootOpener.focus === 'function') _addRootOpener.focus();
  _addRootOpener = null;
}
// Esc closes the add-root modal — parity with the settings/jobs drawers + command palette.
document.addEventListener('keydown', function(e) {
  if (e.key !== 'Escape') return;
  const modal = document.getElementById('add-root-modal');
  if (modal && modal.classList.contains('open')) closeAddRoot();
});
function onRootPathInput(val) {
  clearTimeout(window._fsTimer);
  window._fsTimer = setTimeout(function() { browseFsTo(val); }, 300);
}
async function browseFsTo(path) {
  const browser = document.getElementById('fs-browser');
  if (!browser) return;
  browser.innerHTML = '<div class="fs-entry" style="color:var(--muted)">Loading…</div>';
  try {
    const r = await fetch('/api/fs/ls?path=' + encodeURIComponent(path || ''));
    const d = await r.json();
    if (d && d.error) { browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">' + escapeHtml(d.error) + '</div>'; return; }
    browser.innerHTML = '';
    // /api/fs/ls returns a bare array of directories ({name, path}), with the parent
    // folder included as a leading ".." entry. (Do NOT read `d.entries` — on an array
    // that resolves to Array.prototype.entries, the built-in method, not our data.)
    const entries = Array.isArray(d) ? d : [];
    entries.forEach(function(entry) {
      const el = document.createElement('div');
      el.className = 'fs-entry';
      el.textContent = (entry.name === '..' ? '↑ ' : '') + entry.name;
      el.onclick = function() {
        document.getElementById('add-root-path').value = entry.path;
        browseFsTo(entry.path);
      };
      browser.appendChild(el);
    });
  } catch(e) {
    browser.innerHTML = '<div class="fs-entry" style="color:var(--red)">' + escapeHtml(e.message) + '</div>';
  }
}
async function startIndexRoot() {
  const path = document.getElementById('add-root-path').value.trim();
  if (!path) { toast('Enter a folder path', 'warn'); return; }
  closeAddRoot();
  // A folder is being added → the index is no longer empty; drop the first-run guidance.
  if (typeof applyEmptyState === 'function') applyEmptyState(false);
  try { await fireJob('index', path); }
  catch(e) { toast('Failed: ' + e.message, 'error'); }
}

/* ── Init ── */
loadStats();
loadVersion();
initTree();
// Deep-link restore (v0.37): if location.hash encodes a tab/path/question, restore that
// view instead of the default boot-to-Context. restoreFromHash (a hoisted function
// declaration from 26-url-state.js) is callable here even though 26 executes later —
// only the `window.__indexaRestoreHash` *assignment* runs later, so call the bare name.
// window.__indexaHashRestored guards onboarding from stealing the tab.
window.__indexaHashRestored =
  (typeof restoreFromHash === 'function') && restoreFromHash();
if (!window.__indexaHashRestored) switchTab('tree');
// Land on Context (the file tree + summary) by default, so a returning user sees their
// context — not a blank Ask box. detectEmptyAndOnboard() still tailors the first-run
// welcome for an empty index. Async — no flash for a populated index.
detectEmptyAndOnboard();
reconnectInFlightJobs();
