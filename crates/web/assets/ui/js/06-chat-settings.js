/* ── Chat / Ask ── */
const chat = document.getElementById('chat');
const qInput = document.getElementById('q');
const sendBtn = document.getElementById('send');

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

async function doAsk() {
  const q = qInput.value.trim();
  if (!q) return;

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
        'Select a folder in the sidebar and click <strong>⚡ Build deep context</strong> to index it first.<br>' +
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
  // Render the partial answer (leading whitespace from the model's first token trimmed so
  // it doesn't briefly indent) + sources, keeping the view pinned to the bottom.
  const renderAnswer = function() {
    return renderMarkdown(answerText.replace(/^\s+/, '')) + renderSources(sources);
  };
  const repaint = function() {
    bubble.innerHTML = renderAnswer();
    chat.scrollTop = chat.scrollHeight;
  };
  const handleEvent = function(ev) {
    if (ev.type === 'sources') { sources = ev.sources || []; }
    else if (ev.type === 'fragment') { answerText += ev.text; repaint(); }
    else if (ev.type === 'error') { throw new Error(ev.message || 'Generation failed'); }
    // 'done' is terminal; the loop ends when the stream closes.
  };

  try {
    const r = await fetch('/api/ask/stream', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ question: q })
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
    const errHtml = '<div class="ask-error" style="color:var(--red)">' + escapeHtml(err.message) + '</div>';
    bubble.innerHTML = answerText ? renderAnswer() + errHtml : errHtml;
  }

  sendBtn.disabled = false;
  qInput.focus();
  chat.scrollTop = chat.scrollHeight;
}

sendBtn.addEventListener('click', doAsk);
qInput.addEventListener('keydown', function(e) { if (e.key === 'Enter') doAsk(); });

/* ── Settings ── */
let settingsLoaded = false;
async function loadSettings() {
  if (settingsLoaded) return;
  settingsLoaded = true;
  loadModels();
  loadKeys();
  loadProviderStatus();
  loadPasses();
  loadResource();
  if (typeof loadFeatures === 'function') loadFeatures();
  if (typeof loadPacks === 'function') loadPacks();
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
async function pollQueue() {
  try {
    const r = await fetch('/api/queue');
    const d = await r.json();
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

