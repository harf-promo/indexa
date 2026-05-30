/* ── Summary view ── */
async function showSummary(path) {
  switchTab('tree');
  const view = document.getElementById('summary-view');
  view.style.display = 'block';
  view.innerHTML = '<div class="summary-pending">Loading summary…</div>';

  try {
    const r = await fetch('/api/summary?path=' + encodeURIComponent(path));
    const d = await r.json();

    if (d.error === 'no summary' || d.pending) {
      view.innerHTML = renderNoPendingSummary(path);
      return;
    }
    if (d.error) {
      view.innerHTML = '<div class="summary-pending" style="color:var(--red)">' + escapeHtml(d.error) + '</div>';
      return;
    }

    view.innerHTML = renderSummary(d);

    view.querySelectorAll('.child-item[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    view.querySelectorAll('.crumb[data-path]').forEach(function(el) {
      el.addEventListener('click', function() { showSummary(el.dataset.path); });
    });
    const enqBtn = view.querySelector('#enqueue-btn');
    if (enqBtn) {
      enqBtn.addEventListener('click', async function() {
        enqBtn.disabled = true;
        enqBtn.textContent = 'Queued…';
        await fetch('/api/summarize?path=' + encodeURIComponent(path), { method: 'POST' });
        setTimeout(function() { showSummary(path); }, 2000);
      });
    }
  } catch(e) {
    view.innerHTML = '<div class="summary-pending" style="color:var(--red)">Error: ' + escapeHtml(e.message) + '</div>';
  }
}

function renderNoPendingSummary(path) {
  const name = path.split('/').pop() || path;
  return '<div class="summary-text">' +
    '<div style="color:var(--muted);margin-bottom:12px">No summary yet for <strong>' + escapeHtml(name) + '</strong></div>' +
    '<button class="enqueue-btn" id="enqueue-btn">Generate summary</button>' +
    '</div>';
}

function renderSummary(d) {
  const name = d.path.split('/').pop() || d.path;
  const icon = d.kind === 'dir' ? '📁' : '📄';

  let crumbHtml = '';
  if (d.crumbs && d.crumbs.length) {
    crumbHtml = '<div class="crumbs">' +
      d.crumbs.map(function(c) {
        return '<a class="crumb" data-path="' + escapeAttr(c.path) + '">' + escapeHtml(c.name) + '</a>';
      }).join('<span class="sep">›</span>') +
      '<span class="sep">›</span><span>' + escapeHtml(name) + '</span></div>';
  }

  let childrenHtml = '';
  if (d.children && d.children.length) {
    childrenHtml = '<div class="children-section"><h3>Contents (' + d.children.length + ')</h3>' +
      d.children.map(function(c) {
        const cIcon = c.kind === 'dir' ? '📁' : '📄';
        return '<div class="child-item" data-path="' + escapeAttr(c.path) + '">' +
          '<div class="child-row"><span>' + cIcon + '</span><span class="child-name">' + escapeHtml(c.name) + '</span></div>' +
          '<div class="child-summary">' + escapeHtml(c.summary) + '</div>' +
          '</div>';
      }).join('') + '</div>';
  }

  const ts = d.generated_at ? new Date(d.generated_at * 1000).toLocaleDateString() : '';
  return crumbHtml +
    '<div class="summary-header"><span style="font-size:22px">' + icon + '</span>' +
    '<span class="summary-title">' + escapeHtml(name) + '</span></div>' +
    '<div class="summary-meta">Model: ' + escapeHtml(d.model) + (ts ? ' \xb7 ' + ts : '') + '</div>' +
    '<div class="summary-text">' + escapeHtml(d.summary) + '</div>' +
    childrenHtml;
}

