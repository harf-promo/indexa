/* ── Chat / Ask ── */
const chat = document.getElementById('chat');
const qInput = document.getElementById('q');
const sendBtn = document.getElementById('send');

/* ── Ask scope (file-aware Ask) ───────────────────────────────────────────────
   Selecting a file/folder auto-scopes Ask to it — a removable "Asking about:
   <name> ✕" chip. Clearing → whole-index ask; selecting a new node re-arms it.
   The scope rides as `scope` on /api/ask/stream, mirroring `indexa ask --scope`. */
var askScope = null;       // current path prefix, or null for whole-index
var lastAskQuestion = '';  // remembered for the "broaden to folder" retry

function renderAskScopeChip() {
  var slot = document.getElementById('ask-scope-chip');
  if (!slot) return;
  if (!askScope) { slot.hidden = true; slot.textContent = ''; return; }
  var name = askScope.split('/').pop() || askScope;
  slot.hidden = false;
  slot.innerHTML =
    '<span class="ask-scope-label" title="Answers are limited to paths starting with ' + escapeAttr(askScope) +
    '">Asking about: <strong>' + escapeHtml(name) + '</strong></span>' +
    '<button type="button" class="ask-scope-clear" title="Ask across the whole index" ' +
    'aria-label="Clear scope — ask across the whole index" onclick="clearAskScope()">&#x2715;</button>';
}

// Arm/replace the scope (called when a file/folder is selected). eslint-disable-line no-unused-vars
function setAskScope(path) { askScope = path || null; renderAskScopeChip(); }

// Clear to whole-index. Referenced from the chip's ✕ onclick.
function clearAskScope() { askScope = null; renderAskScopeChip(); }  // eslint-disable-line no-unused-vars

// Bridge from the Context summary header's "Ask about this …" button.
function askAboutSelection(path) {  // eslint-disable-line no-unused-vars
  if (path) setAskScope(path);
  switchTab('chat');
  if (qInput) qInput.focus();
}

function appendMsg(role, html) {
  const welcome = chat.querySelector('.welcome');
  if (welcome) welcome.remove();
  const div = document.createElement('div');
  div.className = 'msg ' + role;
  div.innerHTML = '<div class="bubble">' + html + '</div>';
  chat.appendChild(div);
  chat.scrollTop = chat.scrollHeight;
  return div;
}

/* Render the Sources block appended below an answer. */
function renderSources(sources) {
  if (!sources || !sources.length) return '';
  return '<div class="sources"><h4>Sources</h4>' +
    sources.map(function(s) {
      return '<div class="source-item"><span class="path">' + escapeHtml(s.path) + '</span>' +
        (s.heading ? '<span class="heading">' + escapeHtml(s.heading) + '</span>' : '') +
        '<div class="snippet">' + escapeHtml(s.snippet) + '</div></div>';
    }).join('') + '</div>';
}

// Fetch + render the retrieval trace for the "Why these sources?" expander.
async function loadExplain(body, btn) {
  var q = body.getAttribute('data-q') || '';
  var scope = body.getAttribute('data-scope') || '';
  btn.disabled = true;
  btn.textContent = 'Loading…';
  try {
    var r = await fetch('/api/ask/explain', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ question: q, scope: scope || null }),
    });
    if (!r.ok) throw new Error('HTTP ' + r.status);
    body.innerHTML = renderExplainTrace(await r.json());
  } catch (e) {
    btn.disabled = false;
    btn.textContent = 'Show retrieval breakdown';
    body.insertAdjacentHTML('beforeend', '<div class="explain-err">Couldn’t load: ' + escapeHtml(e.message) + '</div>');
  }
}

function renderExplainTrace(t) {
  var head = '<div class="explain-meta">mode <b>' + escapeHtml(t.mode) + '</b>' +
    ' · top_k ' + t.top_k + (t.rerank ? ' · reranked' : '') + (t.use_weights ? ' · weighted' : '') +
    (t.scope ? ' · scoped to ' + escapeHtml(t.scope) : '') + '</div>';
  var stages = (t.stages || []).map(function (st) {
    var rows = (st.hits || []).map(function (h) {
      var name = (h.path || '').split('/').pop() || h.path;
      return '<li><span class="ex-rank">#' + h.rank + '</span> ' +
        '<span class="ex-path" title="' + escapeAttr(h.path) + '">' + escapeHtml(name) +
        (h.heading ? ' <span class="ex-head">' + escapeHtml(h.heading) + '</span>' : '') + '</span>' +
        '<span class="ex-score">' + (typeof h.score === 'number' ? h.score.toFixed(3) : '') + '</span></li>';
    }).join('');
    return '<div class="explain-stage"><h5>' + escapeHtml(st.label) + '</h5><ol class="explain-hits">' +
      (rows || '<li class="ex-none">(no hits)</li>') + '</ol></div>';
  }).join('');
  return head + stages;
}

// Delegated: the "Show retrieval breakdown" button is rendered inside chat bubbles (re-painted
// during streaming), so bind once on the document rather than per-render.
document.addEventListener('click', function (e) {
  var btn = e.target && e.target.closest ? e.target.closest('.explain-load-btn') : null;
  if (!btn) return;
  var body = btn.closest('.explain-body');
  if (body) loadExplain(body, btn);
});

async function doAsk() {
  const q = qInput.value.trim();
  if (!q) return;
  lastAskQuestion = q; // for the "broaden to folder" retry on thin scoped results
  const scopeForAsk = askScope; // snapshot: the chip may change before the stream returns
  // Reflect the asked question in the URL (deep-linking, v0.37). currentView is 'chat'
  // here (the Ask bar switched to it); writeHash reads lastAskQuestion + askScope.
  if (typeof writeHash === 'function') writeHash();

  // Pre-flight: if no embeddings exist yet, guide the user instead of returning
  // an unhelpful empty/error answer.
  try {
    const statsR = await fetch('/api/stats');
    const stats = await statsR.json();
    if (stats.chunks === 0) {
      switchTab('chat');
      appendMsg('user', escapeHtml(q));
      appendMsg('assistant',
        '<div class="ask-guidance">' +
        '<strong>No context built yet.</strong><br>' +
        'Select a folder in the sidebar and click <strong>⚡ Index for search</strong> to index it first.<br>' +
        '<button class="btn-sm" style="margin-top:10px" onclick="switchTab(\'tree\')">Go to folders →</button>' +
        '</div>');
      return;
    }
  } catch(_) {} // ignore stats failure, proceed normally

  qInput.value = '';
  sendBtn.disabled = true;
  switchTab('chat');

  appendMsg('user', escapeHtml(q));
  const thinking = appendMsg('assistant', '<span class="thinking">Thinking…</span>');
  const bubble = thinking.querySelector('.bubble');

  let answerText = '';
  let sources = [];
  let steps = []; // agentic per-hop queries (empty for one-shot ask)
  let confidence = null; // retrieval-shape confidence, from the terminal 'done' event
  // The agentic retrieval hops, shown as subtle chips above the answer so the user sees
  // what the model searched for while it works.
  const renderSteps = function() {
    if (!steps.length) return '';
    return '<div class="ask-steps">' + steps.map(function(s) {
      return '<span class="ask-step">&#x1F50D; ' + escapeHtml(s.query) + '</span>';
    }).join('') + '</div>';
  };
  // A muted "confidence: medium — 4 moderate matches" line under the answer. Empty until
  // the terminal 'done' event arrives — and stays empty when the server omits the field
  // (no-match answers, older servers), so this is purely additive.
  const renderConfidence = function() {
    if (!confidence || !confidence.level) return '';
    return '<div class="ask-confidence">confidence: ' + escapeHtml(confidence.level) +
      (confidence.basis ? ' — ' + escapeHtml(confidence.basis) : '') + '</div>';
  };
  // Render the partial answer (leading whitespace from the model's first token trimmed so
  // it doesn't briefly indent) + sources, keeping the view pinned to the bottom.
  // "Why these sources?" — a collapsible that, on demand, fetches the retrieval trace for this
  // question (the web `ask --explain`) so the user can see how each source surfaced.
  const renderExplain = function() {
    if (!sources.length) return '';
    return '<details class="explain-trace"><summary class="explain-summary">Why these sources?</summary>' +
      '<div class="explain-body" data-q="' + escapeAttr(q) + '" data-scope="' + escapeAttr(scopeForAsk || '') + '">' +
      '<button class="btn-sm explain-load-btn">Show retrieval breakdown</button></div></details>';
  };
  const renderAnswer = function() {
    return renderSteps() + renderMarkdown(answerText.replace(/^\s+/, '')) +
      renderSources(sources) + renderConfidence() + renderExplain();
  };
  const repaint = function() {
    bubble.innerHTML = renderAnswer();
    chat.scrollTop = chat.scrollHeight;
  };
  const handleEvent = function(ev) {
    if (ev.type === 'sources') { sources = ev.sources || []; }
    else if (ev.type === 'fragment') { answerText += ev.text; repaint(); }
    else if (ev.type === 'step') { steps.push(ev); repaint(); }
    else if (ev.type === 'done') { if (ev.confidence) { confidence = ev.confidence; repaint(); } }
    else if (ev.type === 'error') { throw new Error(ev.message || 'Generation failed'); }
    // 'done' is terminal; the loop ends when the stream closes.
  };

  const agenticEl = document.getElementById('ask-agentic');
  const agentic = agenticEl ? agenticEl.checked : false;

  try {
    const r = await fetch('/api/ask/stream', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ question: q, agentic: agentic, scope: scopeForAsk })
    });
    if (!r.ok || !r.body) throw new Error('Request failed (' + r.status + ')');

    // Parse the text/event-stream body: events are separated by a blank line; we read the
    // `data:` line(s) of each and ignore `:`-comment keep-alives.
    const reader = r.body.getReader();
    const decoder = new TextDecoder();
    let buf = '';
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      buf += decoder.decode(chunk.value, { stream: true });
      let sep;
      while ((sep = buf.indexOf('\n\n')) !== -1) {
        const rawEvent = buf.slice(0, sep);
        buf = buf.slice(sep + 2);
        const data = rawEvent.split('\n')
          .filter(function(l) { return l.indexOf('data:') === 0; })
          .map(function(l) { return l.slice(5).replace(/^ /, ''); })
          .join('\n');
        if (!data) continue;
        let parsed;
        // Skip an unparseable line (e.g. a truncated frame) rather than aborting the whole
        // render; a real `error` event is valid JSON and still throws out of handleEvent.
        try { parsed = JSON.parse(data); } catch (_) { continue; }
        handleEvent(parsed);
      }
    }
    // Guard: a stream that closed without ever producing a fragment (e.g. empty answer).
    if (!answerText) repaint();
  } catch(err) {
    // Keep any already-streamed answer; append the error beneath it rather than discarding.
    const errHtml = '<div class="ask-error" role="alert" style="color:var(--red)">' + escapeHtml(err.message) + '</div>';
    bubble.innerHTML = answerText ? renderAnswer() + errHtml : errHtml;
  }

  // Few results under a single-file/folder scope? Offer to broaden one level up,
  // rather than silently falling back to a whole-index search (which re-introduces
  // the noise scoping was meant to remove). The user stays in control.
  if (scopeForAsk && (sources.length < 3 || (confidence && confidence.level === 'low'))) {
    var parent = scopeForAsk.replace(/\/[^/]+$/, '');
    if (parent && parent !== scopeForAsk) {
      var pName = parent.split('/').pop() || parent;
      var offer = document.createElement('div');
      offer.className = 'ask-broaden';
      offer.appendChild(document.createTextNode('Few results in this scope. '));
      var bBtn = document.createElement('button');
      bBtn.type = 'button';
      bBtn.className = 'btn-sm';
      bBtn.textContent = 'Broaden to ' + pName + '/ →';
      bBtn.addEventListener('click', function() { broadenAskTo(parent); });
      offer.appendChild(bBtn);
      thinking.appendChild(offer);
    }
  }

  sendBtn.disabled = false;
  qInput.focus();
  chat.scrollTop = chat.scrollHeight;
}

// Re-ask the last question with the scope widened to `path` (the parent folder).
function broadenAskTo(path) {
  setAskScope(path);
  if (qInput) { qInput.value = lastAskQuestion; doAsk(); }
}

sendBtn.addEventListener('click', doAsk);
qInput.addEventListener('keydown', function(e) { if (e.key === 'Enter') doAsk(); });

/* ── Settings ── */
let settingsLoaded = false;
async function loadSettings() {
  if (settingsLoaded) return;
  settingsLoaded = true;
  initSettingsAccordion();
  loadModels();
  loadKeys();
  loadProviderStatus();
  loadPasses();
  loadResource();
  if (typeof loadFeatures === 'function') loadFeatures();
  if (typeof loadPacks === 'function') loadPacks();
  if (typeof loadWeights === 'function') loadWeights();
  if (typeof loadImpact === 'function') loadImpact();
}

// Collapse the long Settings drawer into an accordion: each section's <h2> toggles it.
// The first two (Local models, Cloud providers) start open; the rest start collapsed,
// so the drawer is scannable instead of a 400-line scroll.
function initSettingsAccordion() {
  var sections = document.querySelectorAll('.settings-section');
  sections.forEach(function (sec, i) {
    if (i >= 2) sec.classList.add('collapsed');
    var h2 = sec.querySelector('h2');
    if (!h2 || h2.dataset.accordion) return;
    h2.dataset.accordion = '1';
    h2.setAttribute('role', 'button');
    h2.setAttribute('tabindex', '0');
    h2.setAttribute('aria-expanded', i < 2 ? 'true' : 'false');
    var toggle = function () {
      var collapsed = sec.classList.toggle('collapsed');
      h2.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    };
    h2.addEventListener('click', toggle);
    h2.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggle(); }
    });
  });
}

// Expand a collapsed settings section (used by the update-badge deep-link). eslint-disable-line no-unused-vars
function expandSettingsSection(id) {
  var sec = document.getElementById(id);
  if (sec && sec.classList.contains('collapsed')) {
    sec.classList.remove('collapsed');
    var h2 = sec.querySelector('h2');
    if (h2) h2.setAttribute('aria-expanded', 'true');
  }
}
async function loadPasses() {
  try {
    const r = await fetch('/api/config');
    if (!r.ok) return;
    const d = await r.json();
    document.getElementById('passes-first').value = d.passes_first || 2;
    document.getElementById('passes-refresh').value = d.passes_refresh || 1;
  } catch(_) {}
}
async function savePasses() {
  const first = parseInt(document.getElementById('passes-first').value, 10);
  const refresh = parseInt(document.getElementById('passes-refresh').value, 10);
  const status = document.getElementById('passes-status');
  try {
    const r = await fetch('/api/config/passes', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({passes_first: first, passes_refresh: refresh})
    });
    const d = await r.json();
    if (d.error) { status.style.color = 'var(--red)'; status.textContent = d.error; return; }
    status.style.color = 'var(--green)';
    status.textContent = 'Saved';
    setTimeout(function() { status.textContent = ''; }, 3000);
  } catch(e) {
    status.style.color = 'var(--red)';
    status.textContent = 'Error: ' + e.message;
  }
}

async function loadResource() {
  try {
    const r = await fetch('/api/config/resource');
    if (!r.ok) return;
    const d = await r.json();
    document.getElementById('resource-profile').value = d.profile || 'balanced';
    document.getElementById('resource-headroom').value = d.headroom_gb || 0;
  } catch(_) {}
}
async function saveResource() {
  const profile = document.getElementById('resource-profile').value;
  const headroom = parseFloat(document.getElementById('resource-headroom').value) || 0;
  const status = document.getElementById('resource-status');
  try {
    const r = await fetch('/api/config/resource', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({profile: profile, headroom_gb: headroom})
    });
    const d = await r.json();
    if (d.error) { status.style.color = 'var(--red)'; status.textContent = d.error; return; }
    status.style.color = 'var(--green)';
    status.textContent = 'Saved';
    setTimeout(function() { status.textContent = ''; }, 3000);
  } catch(e) {
    status.style.color = 'var(--red)';
    status.textContent = 'Error: ' + e.message;
  }
}

/* ── Queue stats (shown in Jobs tab + sidebar failed badge) ── */
/* "Context not built yet" banner — shown when the index is embedded (chunks>0) but has no
   summaries, so Ask falls back to raw chunks. Auto-hides once summaries exist; dismissible
   for the session. Refreshed alongside the 5 s queue poll. */
var lastQueuePending = 0;
var contextNoticeDismissed = false;
var contextNoticeResolved = false; // summaries confirmed present → stop re-checking

async function refreshContextNotice() {
  var el = document.getElementById('context-notice');
  if (!el) return;
  if (contextNoticeDismissed || contextNoticeResolved) { el.hidden = true; return; }
  try {
    var s = await (await fetch('/api/stats')).json();
    if (s.summaries > 0) { contextNoticeResolved = true; el.hidden = true; return; }
    if (s.chunks === 0) { el.hidden = true; return; } // empty index → onboarding handles it
    el.hidden = false;
    el.innerHTML =
      '<span class="context-notice-msg">&#x1F4A1; <strong>Context not built yet.</strong> ' +
      'Answers fall back to raw file chunks' +
      (lastQueuePending ? ' &mdash; ' + lastQueuePending.toLocaleString() + ' file' +
        (lastQueuePending === 1 ? '' : 's') + ' queued' : '') +
      '. Build summaries for sharper, grounded answers.</span>' +
      '<button type="button" class="btn-sm" onclick="buildContextNow()">Build context</button>' +
      '<button type="button" class="context-notice-x" title="Dismiss" aria-label="Dismiss" onclick="dismissContextNotice()">&#x2715;</button>';
  } catch (_) { el.hidden = true; }
}

function dismissContextNotice() {  // eslint-disable-line no-unused-vars
  contextNoticeDismissed = true;
  var el = document.getElementById('context-notice');
  if (el) el.hidden = true;
}

// Kick off summarization for every root, draining the queue into real summaries.
async function buildContextNow() {  // eslint-disable-line no-unused-vars
  dismissContextNotice();
  try {
    var roots = await (await fetch('/api/roots')).json();
    (roots || []).forEach(function (r) {
      if (typeof fireJob === 'function') fireJob('summarize', r.path);
    });
    if (roots && roots.length && typeof toast === 'function') {
      toast('Building context for ' + roots.length + ' folder' +
        (roots.length === 1 ? '' : 's') + '…', 'info');
    }
  } catch (e) {
    if (typeof toast === 'function') toast('Could not start: ' + e.message, 'error');
  }
}

async function pollQueue() {
  try {
    const r = await fetch('/api/queue');
    const d = await r.json();
    lastQueuePending = d.pending; // for the "context not built" banner's queued-count
    refreshContextNotice();
    // Sidebar failed badge — visible when there are failed summaries
    var badge = document.getElementById('sidebar-failed-badge');
    if (badge) {
      var hasFailed = d.failed > 0;
      badge.hidden = !hasFailed;
      if (hasFailed) badge.textContent = '⚠ ' + d.failed + ' failed';
    }
    // Activity drawer queue row
    const queueEl = document.getElementById('jobs-queue-stats');
    if (!queueEl) return;
    const total = d.pending + d.in_flight + d.failed;
    if (total === 0) {
      queueEl.textContent = 'Summary queue: idle';
      queueEl.style.color = 'var(--muted)';
      return;
    }
    var parts = [];
    if (d.pending > 0) parts.push(d.pending.toLocaleString() + ' pending');
    if (d.in_flight > 0) parts.push(d.in_flight + ' running');
    if (d.failed > 0) parts.push(d.failed + ' failed');
    queueEl.textContent = 'Summary queue: ' + parts.join(' \xb7 ');
    queueEl.style.color = d.failed > 0 ? 'var(--red)' : d.in_flight > 0 ? 'var(--accent)' : 'var(--muted)';
  } catch(_) {}
}
setInterval(pollQueue, 5000);
pollQueue();

/* ── Saved searches (the saved_queries table; `indexa saved` on the CLI) ── */
const savedSel = document.getElementById('saved-q');
const saveQBtn = document.getElementById('save-q');

async function loadSavedQueries() {
  if (!savedSel) return;
  try {
    const r = await fetch('/api/saved');
    if (!r.ok) return;
    const items = await r.json();
    savedSel.innerHTML = '<option value="">&#9733; Saved&#8230;</option>' +
      items.map(function(s) {
        return '<option value="' + escapeHtml(s.name) + '">' + escapeHtml(s.name) + '</option>';
      }).join('');
    savedSel._items = items;
    savedSel.hidden = items.length === 0;
  } catch(_) {}
}

if (savedSel) {
  savedSel.addEventListener('change', function() {
    const name = savedSel.value;
    savedSel.value = '';
    const item = (savedSel._items || []).find(function(s) { return s.name === name; });
    if (!item) return;
    qInput.value = item.question;
    const agenticEl = document.getElementById('ask-agentic');
    if (agenticEl) agenticEl.checked = item.mode === 'agentic';
    doAsk();
  });
  loadSavedQueries();
}

if (saveQBtn) {
  saveQBtn.addEventListener('click', async function() {
    const q = qInput.value.trim();
    if (!q) { qInput.focus(); return; }
    const agenticEl = document.getElementById('ask-agentic');
    // The name IS the (truncated) question — recognizable in the dropdown, and
    // saving the same question again just overwrites its row (upsert by name).
    const name = q.length > 48 ? q.slice(0, 47).trimEnd() + '…' : q;
    try {
      const r = await fetch('/api/saved', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
          name: name,
          question: q,
          mode: (agenticEl && agenticEl.checked) ? 'agentic' : 'rrf'
        })
      });
      if (r.ok) {
        saveQBtn.textContent = '★'; // brief filled-star confirmation
        setTimeout(function() { saveQBtn.textContent = '☆'; }, 1200);
        loadSavedQueries();
      }
    } catch(_) {}
  });
}
