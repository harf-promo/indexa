/* ── Map coverage table view ── */
let mapLoaded = false;
async function loadMap() {
  if (mapLoaded) return;
  mapLoaded = true;
  // Also kick off the treemap (no-op if already loaded)
  if (typeof loadTreemap === 'function') loadTreemap();
  const table = document.getElementById('map-table');
  try {
    const r = await fetch('/api/map');
    const d = await r.json();
    if (!d.total_dirs && !d.total_chunks) {
      table.innerHTML = '<tr><td colspan="2" style="color:var(--muted);padding:12px 10px">No context yet. Add a folder and build deep context first.</td></tr>';
      return;
    }
    // Coverage percentage across directories
    const pct = d.total_dirs > 0 ? Math.round(100 * d.built / d.total_dirs) : 0;
    const rows = [
      { label: '● Built',     value: d.built,         cls: 'cov-full',    desc: 'Folders with AI summaries' },
      { label: '◐ In progress', value: d.partial,     cls: 'cov-partial', desc: 'Queued for summarization' },
      { label: '✗ Failed',    value: d.failed,         cls: 'cov-failed',  desc: 'Summarization failed' },
      { label: '○ Not built', value: d.none,           cls: 'cov-none',    desc: 'No context yet' },
    ];
    table.innerHTML =
      '<thead><tr><th>Coverage</th><th style="text-align:right">Folders</th></tr></thead>';
    const tbody = document.createElement('tbody');
    rows.forEach(function(row) {
      if (row.value === 0 && row.cls !== 'cov-full') return; // hide empty rows except "Built"
      const tr = document.createElement('tr');
      tr.innerHTML = '<td><span class="cov-glyph ' + row.cls + '" style="margin-right:6px"></span>' +
        '<span title="' + escapeHtml(row.desc) + '">' + escapeHtml(row.label) + '</span></td>' +
        '<td style="text-align:right">' + (row.value || 0).toLocaleString() + '</td>';
      tbody.appendChild(tr);
    });
    // Summary footer
    const tfoot = document.createElement('tfoot');
    tfoot.innerHTML =
      '<tr style="border-top:1px solid var(--border)">' +
        '<td style="color:var(--muted);padding-top:8px">Total folders</td>' +
        '<td style="text-align:right;padding-top:8px">' + (d.total_dirs||0).toLocaleString() + '</td>' +
      '</tr>' +
      '<tr>' +
        '<td style="color:var(--muted)">Total chunks</td>' +
        '<td style="text-align:right">' + (d.total_chunks||0).toLocaleString() + '</td>' +
      '</tr>' +
      '<tr>' +
        '<td style="color:var(--muted)">Coverage</td>' +
        '<td style="text-align:right;font-weight:600;color:var(--accent)">' + pct + '%</td>' +
      '</tr>';
    table.appendChild(tbody);
    table.appendChild(tfoot);
  } catch(e) {
    table.innerHTML = '<tr><td style="color:var(--red)">' + escapeHtml(e.message) + '</td></tr>';
  }
}

/* ── Local models (Ollama): rich installed ∪ catalog rows from /api/models ── */
// Active per-role assignments, read from /api/config; used to light up live rows.
var activeModelCfg = {};

// Bare name == :latest variant — match the backend's dedup.
function normModel(name) {
  return String(name).replace(/:latest$/, '');
}
// Params in billions → "12.2B" / "137M" (embedders are sub-1B).
function fmtParams(b) {
  if (!b || b <= 0) return '';
  return b >= 1 ? (b < 10 ? b.toFixed(1) : Math.round(b)) + 'B' : Math.round(b * 1000) + 'M';
}
// Bytes → "7.6 GB". Local copy: 09-engine.js's fmtGB is IIFE-scoped, not global.
function fmtBytesGB(bytes) {
  var gb = (bytes || 0) / 1073741824;
  return gb.toFixed(gb < 10 ? 1 : 0) + ' GB';
}

async function loadModels() {
  const list = document.getElementById('models-list');
  try {
    const [cfgR, modR] = await Promise.all([fetch('/api/config'), fetch('/api/models')]);
    activeModelCfg = cfgR.ok ? await cfgR.json() : {};
    const endpointEl = document.getElementById('ollama-endpoint');
    if (endpointEl && !endpointEl.value) endpointEl.value = activeModelCfg.base_url || '';
    const data = await modR.json();
    if (data.error) throw new Error(data.error);
    const models = data.models || [];

    // "No job selected" line: budget + the active dir model's whole-index ETA.
    const budgetEl = document.getElementById('models-budget');
    if (budgetEl) {
      const dir = models.find(function(m) { return normModel(m.name) === normModel(activeModelCfg.dir_model || ''); });
      var line = 'Memory budget ' + fmtBytesGB(Math.max(0, data.budget_bytes || 0));
      if (dir && dir.eta_display) line += ' · summarize your whole index with ' + escapeHtml(dir.name) + ' ≈ ' + escapeHtml(dir.eta_display);
      budgetEl.innerHTML = line;
    }

    const installed = models.filter(function(m) { return m.installed; });
    const avail = models.filter(function(m) { return !m.installed; });
    var html = '';
    html += '<div class="model-subhead">Installed</div>';
    html += installed.length
      ? installed.map(renderModelRow).join('')
      : '<div class="model-empty">No models installed. Pull one below.</div>';
    if (avail.length) {
      html += '<div class="model-subhead">Available to download</div>';
      html += avail.map(renderModelRow).join('');
    }
    list.innerHTML = html;
  } catch(e) {
    list.innerHTML = '<div class="model-empty" style="color:var(--red)">Ollama not reachable: ' + escapeHtml(e.message) + '</div>';
  }
}

function renderModelRow(m) {
  const isEmbed = m.role === 'embed';
  const norm = normModel(m.name);
  const fitCls = m.fits ? 'fit-ok' : 'fit-warn';
  const fitTxt = m.fits ? '✅ fits' : '⚠ tight';
  const size = m.size_bytes > 0 ? fmtBytesGB(m.size_bytes) + (m.size_is_estimate ? ' est' : '') : '';
  const params = fmtParams(m.params_b);
  const meta = [size, params, (m.installed ? '' : m.vendor)].filter(Boolean).join(' · ');
  // Which roles is this model currently assigned to?
  var activeChips = '';
  if (norm === normModel(activeModelCfg.file_model || '')) activeChips += '<span class="active-role">file</span>';
  if (norm === normModel(activeModelCfg.dir_model || '')) activeChips += '<span class="active-role">dir</span>';
  if (norm === normModel(activeModelCfg.embed_model || '')) activeChips += '<span class="active-role">embed</span>';

  var actions = '';
  if (m.installed) {
    if (isEmbed) {
      actions = '<button class="btn-sm" onclick="setModelRole(this.closest(\'.model-row\').dataset.name,\'embed\')">Set embedder</button>';
    } else {
      actions = '<button class="btn-sm" onclick="setModelRole(this.closest(\'.model-row\').dataset.name,\'file\')">Set file</button>' +
        '<button class="btn-sm" onclick="setModelRole(this.closest(\'.model-row\').dataset.name,\'dir\')">Set dir</button>';
    }
  } else {
    actions = '<button class="btn-sm" onclick="pullModelNamed(this.closest(\'.model-row\').dataset.name)">Pull</button>';
  }
  const flag = m.safe_default === false ? '<span class="vendor-flag" title="Listed but not a recommended default per vendor policy">⚠</span>' : '';

  return '<div class="model-row' + (m.installed ? '' : ' avail') + '" data-name="' + escapeAttr(m.name) + '">' +
    '<span class="model-name">' + escapeHtml(m.name) + flag + '</span>' +
    '<span class="role-chip">' + escapeHtml(m.role) + '</span>' +
    activeChips +
    '<span class="model-meta">' + escapeHtml(meta) + '</span>' +
    '<span class="fit-badge ' + fitCls + '">' + fitTxt + '</span>' +
    '<span class="model-eta">' + escapeHtml(m.eta_display || '') + '</span>' +
    '<span class="model-actions">' + actions + '</span>' +
    '</div>';
}

async function setModelRole(name, role) {
  if (role === 'embed') {
    if (!confirm('Set the embedding model to "' + name + '"?\n\nThis applies on the next re-embed (indexa deep). An embedder with a different vector dimension makes your existing index incompatible — search stays wrong until a full re-embed.')) return;
  }
  const field = role === 'file' ? 'file_model' : role === 'dir' ? 'dir_model' : 'embed_model';
  const body = {}; body[field] = name;
  try {
    const r = await fetch('/api/config/provider', {
      method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)
    });
    if (!r.ok) { toast('Failed to set model (' + r.status + ')', 'error'); return; }
    const d = await r.json();
    if (d.error) { toast(d.error, 'error'); return; }
    toast('Set ' + role + ' model → ' + name + (d.restart_required ? ' · restart indexa to apply' : ''), 'info');
    loadModels();
  } catch(e) { toast('Error: ' + e.message, 'error'); }
}

async function refreshCatalog() {
  const btn = document.getElementById('refresh-catalog-btn');
  if (btn) btn.disabled = true;
  try {
    const r = await fetch('/api/models/catalog/refresh', {method: 'POST'});
    const d = await r.json();
    if (d.refreshed) toast('Catalog refreshed (' + (d.count || 0) + ' models)', 'info');
    else toast(d.reason || d.error || 'Catalog not refreshed', 'warn');
    loadModels();
  } catch(e) { toast('Error: ' + e.message, 'error'); }
  if (btn) btn.disabled = false;
}

// Streaming pull core, shared by the manual input and per-row Pull buttons.
async function pullModelNamed(name) {
  name = (name || '').trim();
  if (!name) return;
  const btn = document.getElementById('pull-btn');
  const prog = document.getElementById('pull-progress');
  if (btn) btn.disabled = true;
  prog.style.display = 'block';
  prog.textContent = 'Starting pull for ' + name + '…\n';
  try {
    const r = await fetch('/api/models/pull', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({name: name})
    });
    if (!r.ok) { const d = await r.json(); throw new Error(d.error || 'Failed'); }
    const reader = r.body.getReader();
    const dec = new TextDecoder();
    while (true) {
      const {done, value} = await reader.read();
      if (done) break;
      const lines = dec.decode(value, {stream: true}).split('\n').filter(Boolean);
      lines.forEach(function(line) {
        try {
          const d = JSON.parse(line);
          if (d.status) prog.textContent += d.status + (d.completed ? ' ' + d.completed : '') + '\n';
          prog.scrollTop = prog.scrollHeight;
        } catch(_) {}
      });
    }
    prog.textContent += '✓ Done.\n';
    const input = document.getElementById('pull-input');
    if (input) input.value = '';
    setTimeout(loadModels, 500);
  } catch(e) {
    prog.textContent += '✗ Error: ' + e.message + '\n';
  }
  if (btn) btn.disabled = false;
}

function pullModel() {
  const input = document.getElementById('pull-input');
  pullModelNamed(input ? input.value : '');
}

// Provider switch + Ollama endpoint write-back (POST /api/config/provider).
async function setProvider(provider, model) {
  const body = {provider: provider};
  if (model) body.model = model;
  try {
    const r = await fetch('/api/config/provider', {
      method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)
    });
    if (!r.ok) { toast('Failed to set provider (' + r.status + ')', 'error'); return; }
    const d = await r.json();
    if (d.error) { toast(d.error, 'error'); return; }
    toast('Provider → ' + provider + (d.restart_required ? ' · restart indexa to apply' : ''), 'info');
    loadProviderStatus();
  } catch(e) { toast('Error: ' + e.message, 'error'); }
}
function useClaude() { setProvider('claude-code', 'sonnet'); }
function useLocalOllama() { setProvider('ollama', ''); }

async function saveEndpoint() {
  const el = document.getElementById('ollama-endpoint');
  const url = el ? el.value.trim() : '';
  if (!url) return;
  try {
    const r = await fetch('/api/config/provider', {
      method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({base_url: url})
    });
    if (!r.ok) { toast('Failed to save endpoint (' + r.status + ')', 'error'); return; }
    const d = await r.json();
    if (d.error) { toast(d.error, 'error'); return; }
    toast('Ollama endpoint saved' + (d.restart_required ? ' · restart indexa to apply' : ''), 'info');
    loadModels();
  } catch(e) { toast('Error: ' + e.message, 'error'); }
}

async function loadKeys() {
  try {
    const r = await fetch('/api/keys');
    if (r.status === 403) {
      document.getElementById('key-gate-notice').style.display = 'block';
      ['openai','anthropic','google'].forEach(function(p) {
        document.getElementById('key-' + p).disabled = true;
      });
      return;
    }
    const d = await r.json();
    document.getElementById('badge-openai').textContent = d.openai_set ? '✓ set' : '';
    document.getElementById('badge-anthropic').textContent = d.anthropic_set ? '✓ set' : '';
    document.getElementById('badge-google').textContent = d.google_set ? '✓ set' : '';
  } catch(_) {}
}

async function loadProviderStatus() {
  try {
    const r = await fetch('/api/providers/status');
    if (!r.ok) return;
    const d = await r.json();
    const cli = document.getElementById('badge-claude-cli');
    const auth = document.getElementById('badge-claude-auth');
    const active = document.getElementById('badge-claude-active');
    if (cli) cli.textContent = d.claude_cli_present
      ? ('✓ ' + (d.claude_cli_version ? 'v' + d.claude_cli_version : 'found'))
      : '✗ not found';
    if (auth) {
      if (d.claude_logged_in) {
        auth.textContent = '✓ ' + (d.claude_subscription ? d.claude_subscription + ' plan' : 'signed in');
      } else {
        auth.textContent = d.claude_cli_present ? '✗ run: claude login' : '—';
      }
    }
    if (active) active.textContent = d.describer_provider === 'claude-code'
      ? '✓ claude-code'
      : (d.describer_provider || '—');
    // Show the provider switch that moves AWAY from the current provider.
    const useClaudeBtn = document.getElementById('use-claude-btn');
    const useLocalBtn = document.getElementById('use-local-btn');
    const onClaude = d.describer_provider === 'claude-code';
    if (useClaudeBtn) useClaudeBtn.style.display = onClaude ? 'none' : '';
    if (useLocalBtn) useLocalBtn.style.display = onClaude ? '' : 'none';
  } catch(_) {}
}

async function saveKey(provider) {
  const val = document.getElementById('key-' + provider).value.trim();
  if (!val) return clearKey(provider);
  try {
    const r = await fetch('/api/keys', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({provider: provider, key: val})
    });
    if (!r.ok) { toast('Failed to save key (' + r.status + ')', 'error'); return; }
    const d = await r.json();
    if (d.error) { toast(d.error, 'error'); return; }
    document.getElementById('key-' + provider).value = '';
    loadKeys();
  } catch(e) { toast('Error saving key: ' + e.message, 'error'); }
}

async function clearKey(provider) {
  try {
    const r = await fetch('/api/keys', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({provider: provider, key: ''})
    });
    if (!r.ok) { toast('Failed to clear key (' + r.status + ')', 'error'); return; }
    const d = await r.json();
    if (d.error) { toast(d.error, 'error'); return; }
    loadKeys();
  } catch(e) { toast('Error clearing key: ' + e.message, 'error'); }
}


/* ── Advanced features (ANN / image / audio) ── */
async function loadFeatures() {
  try {
    const r = await fetch('/api/config/features');
    if (!r.ok) return;
    const d = await r.json();
    var ann = document.getElementById('feat-ann');
    var annMin = document.getElementById('feat-ann-min-chunks');
    var imgCap = document.getElementById('feat-image-caption');
    var imgModel = document.getElementById('feat-image-model');
    var audTx = document.getElementById('feat-audio-transcribe');
    var audBin = document.getElementById('feat-audio-binary');
    if (ann)     ann.checked = !!d.ann;
    if (annMin)  annMin.value = d.ann_min_chunks || 50000;
    if (imgCap)  imgCap.checked = !!d.image_caption;
    if (imgModel && d.image_model) imgModel.value = d.image_model;
    if (audTx)   audTx.checked = !!d.audio_transcribe;
    if (audBin && d.audio_binary) audBin.value = d.audio_binary;
  } catch(_) {}
}

async function saveFeatures() {
  var status = document.getElementById('features-status');
  var body = {
    ann:              document.getElementById('feat-ann')?.checked,
    ann_min_chunks:   parseInt(document.getElementById('feat-ann-min-chunks')?.value, 10) || 50000,
    image_caption:    document.getElementById('feat-image-caption')?.checked,
    image_model:      (document.getElementById('feat-image-model')?.value || '').trim() || null,
    audio_transcribe: document.getElementById('feat-audio-transcribe')?.checked,
    audio_binary:     (document.getElementById('feat-audio-binary')?.value || '').trim() || null,
  };
  try {
    var r = await fetch('/api/config/features', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(body)
    });
    var d = await r.json();
    if (d.error) { if (status) { status.style.color = 'var(--red)'; status.textContent = d.error; } return; }
    if (status) {
      status.style.color = 'var(--green)';
      status.textContent = 'Saved' + (d.restart_required ? ' · restart indexa to apply' : '');
      setTimeout(function() { if (status) status.textContent = ''; }, 4000);
    }
  } catch(e) {
    if (status) { status.style.color = 'var(--red)'; status.textContent = 'Error: ' + e.message; }
  }
}
