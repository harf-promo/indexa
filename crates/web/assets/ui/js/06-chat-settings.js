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

/* ── Conversational Ask (multi-turn) ──────────────────────────────────────────
   Each conversation has a client-generated id sent as `session_id` on every ask.
   The server folds the session's recent turns into the prompt + rewrites the
   follow-up into a standalone query, so "and why is that?" resolves against the
   thread. The #chat area already keeps every turn's bubble within a page load;
   "＋ New" resets the id and clears the thread. */
var askSessionId = null;
function ensureAskSession() {
  if (!askSessionId) {
    askSessionId = (window.crypto && crypto.randomUUID)
      ? crypto.randomUUID()
      : 'sess-' + Date.now().toString(36) + '-' + Math.floor(Math.random() * 1e9).toString(36);
  }
  return askSessionId;
}
// Start a fresh conversation: drop the id and reset the thread to the welcome state.
function newConversation() {  // eslint-disable-line no-unused-vars
  askSessionId = null;
  chat.innerHTML =
    '<div class="welcome"><h2>Ask your local context</h2>' +
    '<p>New conversation. Ask a question about your files in plain language — ' +
    'follow-ups remember what you just asked.</p></div>';
  var si = document.getElementById('ask-session-impact');
  if (si) { si.hidden = true; si.innerHTML = ''; }
  if (qInput) qInput.focus();
}

// After each answered turn, refresh the conversation-level RUNNING savings badge from the
// authoritative per-session ledger (`GET /api/session-impact/{id}`) — making the core pitch
// ("Indexa serves a slice, not whole files") visible as it accumulates across the conversation.
// Read-only + fail-open: any error, an unmeaningful total (served not smaller), or no calls yet
// just hides the badge.
function updateSessionImpact() {
  var el = document.getElementById('ask-session-impact');
  if (!el || !askSessionId) return;
  var fmt = function (n) {
    if (n >= 1073741824) return (n / 1073741824).toFixed(1) + ' GB';
    if (n >= 1048576) return (n / 1048576).toFixed(1) + ' MB';
    if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
    return n + ' B';
  };
  fetch('/api/session-impact/' + encodeURIComponent(askSessionId))
    .then(function (r) { return r.ok ? r.json() : null; })
    .then(function (d) {
      if (!d || !d.counterfactual || d.served >= d.counterfactual || (d.calls || 0) < 1) {
        el.hidden = true;
        return;
      }
      var pct = Math.min(99, Math.round((1 - d.served / d.counterfactual) * 100));
      var savedBytes = Math.max(0, (d.counterfactual || 0) - (d.served || 0));
      var tokStr = savedBytes > 0 ? (function(t) {
        if (t >= 1000000) return '~' + (t / 1000000).toFixed(1) + 'M';
        if (t >= 1000) return '~' + (t / 1000).toFixed(1) + 'K';
        return '~' + String(t);
      })(Math.round(savedBytes / 4)) + ' tokens' : '';
      el.hidden = false;
      el.innerHTML =
        '\u{1F4C9} This conversation: served <strong>' + escapeHtml(fmt(d.served)) +
        '</strong> vs <strong>' + escapeHtml(fmt(d.counterfactual)) + '</strong> of source \u{2014} <strong>' +
        pct + '% less</strong>' + (tokStr ? ' (' + escapeHtml(tokStr) + ')' : '') +
        ' across ' + d.calls + (d.calls === 1 ? ' answer' : ' answers');
    })
    .catch(function () { el.hidden = true; });
}

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
  var caption = '<p class="explain-caption">How Indexa found these files:</p>';
  var head = caption + '<div class="explain-meta">mode <b>' + escapeHtml(t.mode) + '</b>' +
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
        'Select a folder in the sidebar and click <strong>' + ICO_BOLT + ' Index for search</strong> to index it first.<br>' +
        '<button class="btn-sm" style="margin-top:10px" onclick="switchTab(\'tree\')">Go to folders →</button>' +
        '</div>');
      return;
    }
  } catch(_) {} // ignore stats failure, proceed normally

  qInput.value = '';
  sendBtn.disabled = true;
  sendBtn.textContent = 'Asking…';
  switchTab('chat');

  appendMsg('user', escapeHtml(q));
  const thinking = appendMsg('assistant', '<span class="thinking">Thinking…</span>');
  const bubble = thinking.querySelector('.bubble');

  let answerText = '';
  let sources = [];
  let steps = []; // agentic per-hop queries (empty for one-shot ask)
  let confidence = null; // retrieval-shape confidence, from the terminal 'done' event
  let impact = null; // per-answer byte savings vs whole-file context, from the 'done' event
  // The agentic retrieval hops, shown as subtle chips above the answer so the user sees
  // what the model searched for while it works.
  const renderSteps = function() {
    if (!steps.length) return '';
    return '<div class="ask-steps">' + steps.map(function(s) {
      return '<span class="ask-step">' + ICO_SEARCH + ' ' + escapeHtml(s.query) + '</span>';
    }).join('') + '</div>';
  };
  // A muted "confidence: medium — 4 moderate matches" line under the answer. Empty until
  // the terminal 'done' event arrives — and stays empty when the server omits the field
  // (no-match answers, older servers), so this is purely additive.
  const renderConfidence = function() {
    if (!confidence || !confidence.level) return '';
    var gaps = (confidence.uncovered && confidence.uncovered.length)
      ? '<div class="ask-uncovered" title="Question terms found in none of the cited sources — the answer may be partial.">may not cover: ' +
        escapeHtml(confidence.uncovered.join(', ')) + '</div>'
      : '';
    return '<div class="ask-confidence">retrieval coverage: ' + escapeHtml(confidence.level) +
      (confidence.basis ? ' — ' + escapeHtml(confidence.basis) : '') + '</div>' + gaps;
  };
  // Binary byte size (matches the server's human_bytes: 1 decimal, KB/MB/GB).
  const fmtBytes = function(n) {
    if (n >= 1073741824) return (n / 1073741824).toFixed(1) + ' GB';
    if (n >= 1048576) return (n / 1048576).toFixed(1) + ' MB';
    if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
    return n + ' B';
  };
  // The "retrieve the slice" win, made concrete for THIS answer: how much smaller the served
  // context was than pasting the cited files whole. Absent (empty) on no-match answers and
  // older servers — purely additive, like the confidence line.
  const fmtTokens = function(saved) {
    // bytes/4 ≈ tokens — same basis as UsageSummary::savings_line and AnswerImpact::human().
    var t = Math.round(Math.max(0, saved) / 4);
    if (t >= 1000000) return (t / 1000000).toFixed(1) + 'M';
    if (t >= 1000) return (t / 1000).toFixed(1) + 'K';
    return String(t);
  };
  // "Show the math" — the per-file breakdown behind the one-liner: each cited file's full
  // on-disk size (largest first) that Indexa served a retrieved slice of instead. Present only
  // when the server sent `impact.items` (additive; older servers omit it).
  const renderSavingsTable = function() {
    if (!impact || !impact.items || !impact.items.length) return '';
    var rows = impact.items.slice().sort(function(a, b) {
      return (b.source_bytes || 0) - (a.source_bytes || 0);
    });
    var body = '';
    for (var i = 0; i < rows.length; i++) {
      body += '<tr><td>' + escapeHtml(rows[i].path || '') + '</td>' +
        '<td class="impact-num">' + escapeHtml(fmtBytes(rows[i].source_bytes || 0)) + '</td></tr>';
    }
    return '<details class="ask-impact-details"><summary>Show the math</summary>' +
      '<table class="impact-table"><thead><tr><th>Cited file</th><th>Full source</th></tr></thead>' +
      '<tbody>' + body + '</tbody></table></details>';
  };
  const renderImpact = function() {
    if (!impact || !impact.saved_percent) return '';
    var saved = (impact.counterfactual_bytes || 0) - (impact.served_bytes || 0);
    var tokLine = saved > 0
      ? ' (~' + escapeHtml(fmtTokens(saved)) + ' tokens at \u{2248}4 bytes/token)'
      : '';
    return '<div class="ask-impact">' +
      '\u{1F4C9} served ' + escapeHtml(fmtBytes(impact.served_bytes)) + ' vs ' +
      escapeHtml(fmtBytes(impact.counterfactual_bytes)) + ' of source \u{2014} <strong>' +
      escapeHtml(String(impact.saved_percent)) + '% less</strong>' + escapeHtml(tokLine) +
      '<details class="ask-impact-details"><summary>How is this measured?</summary>' +
      '<p>Indexa retrieved only the relevant slices of your cited files instead of their full content. ' +
      '<strong>Served</strong> = the answer text + the snippets actually delivered. ' +
      '<strong>Source</strong> = the on-disk size of the cited files only (not the whole repo) — ' +
      'so the saving is conservative. The token estimate uses \u{2248}4 bytes per token.</p>' +
      '</details>' + renderSavingsTable() + '</div>';
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
  // When the question was scoped to a file/folder, the answer only saw that subtree — so a
  // thin or "nothing found" reply might just mean the scope was too narrow. Surface the way
  // out inline (the ✕ chip already clears it) so a scoped dead-end isn't mistaken for "Indexa
  // doesn't know".
  const renderScopeHint = function() {
    if (!scopeForAsk) return '';
    var name = scopeForAsk.split('/').filter(Boolean).pop() || scopeForAsk;
    return '<div class="ask-scope-hint" style="margin-top:8px;color:var(--muted);font-size:12px">' +
      '&#x21B3; Answered within <strong>' + escapeHtml(name) + '</strong> only — ' +
      'clear the scope (the &#x2715; chip above the question box) to search everywhere.</div>';
  };
  const renderAnswer = function() {
    return renderSteps() + renderMarkdown(answerText.replace(/^\s+/, '')) +
      renderSources(sources) + renderConfidence() + renderImpact() + renderExplain() + renderScopeHint();
  };
  const repaint = function() {
    bubble.innerHTML = renderAnswer();
    chat.scrollTop = chat.scrollHeight;
  };
  const handleEvent = function(ev) {
    if (ev.type === 'sources') { sources = ev.sources || []; }
    else if (ev.type === 'fragment') { answerText += ev.text; repaint(); }
    else if (ev.type === 'step') { steps.push(ev); repaint(); }
    else if (ev.type === 'done') {
      if (ev.confidence) confidence = ev.confidence;
      if (ev.impact) impact = ev.impact;
      if (ev.confidence || ev.impact) repaint();
      // Refresh the conversation-level running savings badge from the per-session ledger.
      updateSessionImpact();
    }
    else if (ev.type === 'error') { throw new Error(ev.message || 'Generation failed'); }
    // 'done' is terminal; the loop ends when the stream closes.
  };

  const agenticEl = document.getElementById('ask-agentic');
  const agentic = agenticEl ? agenticEl.checked : false;

  try {
    const r = await fetch('/api/ask/stream', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ question: q, agentic: agentic, scope: scopeForAsk, session_id: ensureAskSession() })
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
  sendBtn.textContent = 'Ask';
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
  const btn = document.querySelector('button[onclick="savePasses()"]');
  if (btn) btn.disabled = true;
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
  } finally {
    if (btn) btn.disabled = false;
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
  const btn = document.querySelector('button[onclick="saveResource()"]');
  if (btn) btn.disabled = true;
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
  } finally {
    if (btn) btn.disabled = false;
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
      '<span class="context-notice-msg"><strong>Context not built yet.</strong> ' +
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
