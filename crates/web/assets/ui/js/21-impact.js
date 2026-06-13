/* ── Impact dashboard (token-savings telemetry) ────────────────────────────────
   Promotes the engine-bar "Saved ~N tok/wk" widget into a full Settings panel:
   weekly tokens saved + a per-tool breakdown (ask / search / get_summary /
   read_file …). Lazily loaded by loadSettings() when the drawer opens. Every
   number is an ESTIMATE (≈4 bytes/token); the basis line carries the same honest
   caveat as `indexa status` and MCP get_stats — one source of truth in
   UsageSummary::savings_line. eslint-disable-line no-unused-vars */
async function loadImpact() {
  var body = document.getElementById('impact-body');
  if (!body) return;
  try {
    var r = await fetch('/api/impact');
    if (!r.ok) throw new Error('http ' + r.status);
    renderImpact(body, await r.json());
  } catch (_) {
    body.textContent = '';
    var p = document.createElement('p');
    p.className = 'settings-note';
    p.textContent = 'Could not load impact data.';
    body.appendChild(p);
  }
}

// bytes/4 ≈ tokens — same basis as UsageSummary::savings_line + the engine widget.
function impactTokens(bytes) {
  return Math.round(Math.max(0, bytes || 0) / 4).toLocaleString();
}
function impactSize(bytes) {
  bytes = bytes || 0;
  if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + ' MB';
  if (bytes >= 1024) return (bytes / 1024).toFixed(1) + ' KB';
  return bytes + ' B';
}

function renderImpact(body, d) {
  body.textContent = '';
  if (!d || !d.calls) {
    var empty = document.createElement('p');
    empty.className = 'settings-note';
    empty.textContent = 'No retrieval activity yet this week. Ask a question or run a search, then check back.';
    body.appendChild(empty);
    return;
  }
  var saved = (d.counterfactual || 0) - (d.served || 0);

  var headline = document.createElement('div');
  headline.className = 'impact-headline';
  headline.appendChild(document.createTextNode('~' + impactTokens(saved) + ' tokens '));
  var sub = document.createElement('span');
  sub.className = 'impact-sub';
  sub.textContent = 'saved this week';
  headline.appendChild(sub);
  body.appendChild(headline);

  if (d.savings_line) {
    var basis = document.createElement('p');
    basis.className = 'impact-basis';
    basis.textContent = d.savings_line;
    body.appendChild(basis);
  }

  var tools = d.by_tool || [];
  if (tools.length) {
    var table = document.createElement('table');
    table.className = 'impact-table';
    var thead = document.createElement('thead');
    thead.innerHTML = '<tr><th>Tool</th><th>Calls</th><th>Served</th><th>Tokens saved</th></tr>';
    table.appendChild(thead);
    var tbody = document.createElement('tbody');
    tools.forEach(function (t) {
      var ts = (t.counterfactual || 0) - (t.served || 0);
      var tr = document.createElement('tr');
      // Tool name is a fixed server constant; numbers are formatted — build with
      // textContent regardless so the table can never reflect untrusted markup.
      var cells = [t.tool, (t.calls || 0).toLocaleString(), impactSize(t.served), '~' + impactTokens(ts)];
      cells.forEach(function (c, i) {
        var td = document.createElement('td');
        td.textContent = c;
        if (i > 0) td.className = 'impact-num';
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    body.appendChild(table);
  }
}
