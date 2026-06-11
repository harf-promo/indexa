// ── Review inbox (v0.22 Decision Ledger) ──────────────────────────────────────
// Lists open ledger questions (GET /api/review) in the 'review' drawer.
// Answering routes through the same server-side entry point as CLI/MCP
// (decide_and_apply), so the web surface inherits the projection contract.

document.addEventListener('DOMContentLoaded', loadReviewCount);
setInterval(loadReviewCount, 15000);

function loadReviewCount() {
  fetch('/api/review/count')
    .then(function (r) { return r.json(); })
    .then(function (d) {
      var badge = document.getElementById('review-badge');
      if (!badge) return;
      var n = (d && d.open) ? d.open : 0;
      badge.textContent = n;
      badge.hidden = n === 0;
      // The aria-label is the button's whole accessible name — fold the count
      // in, or screen readers never hear that questions exist.
      var btn = badge.closest('button');
      if (btn) {
        btn.setAttribute('aria-label',
          n > 0 ? 'Open review inbox, ' + n + ' open question' + (n === 1 ? '' : 's')
                : 'Open review inbox');
      }
    })
    .catch(function () {});
}

function loadReview() {  // eslint-disable-line no-unused-vars
  var list = document.getElementById('review-list');
  if (!list) return;
  fetch('/api/review')
    .then(function (r) { return r.json(); })
    .then(renderReviewList)
    .catch(function (e) {
      list.textContent = '';
      var err = document.createElement('div');
      err.className = 'review-empty';
      err.textContent = 'Failed to load questions: ' + e.message;
      list.appendChild(err);
    });
}

function renderReviewList(questions) {
  var list = document.getElementById('review-list');
  if (!list) return;
  list.textContent = '';
  if (!questions || !questions.length) {
    renderReviewEmpty(list);
    return;
  }
  questions.forEach(function (q) { list.appendChild(buildReviewCard(q)); });
}

function renderReviewEmpty(list) {
  var el = document.createElement('div');
  el.className = 'review-empty';
  el.textContent = 'Inbox zero — nothing needs your judgment.';
  list.appendChild(el);
}

/* One question card. Built with createElement/textContent throughout — titles,
   details, and option labels embed user file paths, so nothing user-derived is
   ever interpolated as HTML. */
function buildReviewCard(q) {
  var card = document.createElement('div');
  // priority ≥ 100 = re-ask (contradicts a prior user answer) — accent stripe.
  card.className = 'review-card' + (q.priority >= 100 ? ' reask' : '');

  var title = document.createElement('div');
  title.className = 'review-card-title';
  title.textContent = q.title;
  card.appendChild(title);

  var detail = document.createElement('div');
  detail.className = 'review-card-detail';
  detail.textContent = q.detail;
  card.appendChild(detail);

  var row = document.createElement('div');
  row.className = 'review-card-options';
  // options are [value, label] pairs; value is what the answer API expects back.
  (q.options || []).forEach(function (opt) {
    var btn = document.createElement('button');
    btn.className = 'btn-sm';
    // Middle-truncate long labels (paths): end-ellipsis renders
    // report-final.pdf and report-final-2.pdf as identical buttons — the
    // differentiating tail must stay visible. Done in JS rather than CSS
    // direction tricks so RTL characters in file names can't scramble.
    btn.textContent = middleTruncate(opt[1], 58);
    btn.title = opt[0];
    btn.onclick = function () { answerReview(q.id, opt[0], card); };
    row.appendChild(btn);
  });
  var dismiss = document.createElement('button');
  dismiss.className = 'review-dismiss';
  dismiss.textContent = 'Dismiss';
  dismiss.title = 'Stop asking — this question only returns if the evidence changes';
  dismiss.onclick = function () { dismissReview(q.id, card); };
  row.appendChild(dismiss);

  // Time-travel (v0.25): toggle the subject's full revision chain inline.
  var hist = document.createElement('button');
  hist.className = 'review-history-btn';
  hist.textContent = 'History';
  hist.title = 'Every decision recorded for this subject, oldest first';
  hist.setAttribute('aria-expanded', 'false');
  hist.onclick = function () { toggleReviewHistory(card, q, hist); };
  row.appendChild(hist);

  card.appendChild(row);
  return card;
}

/* ── Time-travel: per-subject revision chain ──────────────────────────────────
   Same XSS rule as the cards: every value from the API (subjects, chosen
   answers, types are user file paths / symbols) is rendered via textContent,
   never as HTML. The history endpoint walks ALL decision types for the
   subject, so archive/duplicate chains render here too. */
function toggleReviewHistory(card, q, btn) {
  var existing = card.querySelector('.review-history');
  if (existing) { existing.remove(); btn.setAttribute('aria-expanded', 'false'); return; }
  var box = document.createElement('div');
  box.className = 'review-history';
  box.textContent = 'Loading history…';
  card.appendChild(box);
  btn.setAttribute('aria-expanded', 'true');
  fetch('/api/review/history?subject=' + encodeURIComponent(q.subject))
    .then(function (r) { return r.json(); })
    .then(function (rows) { renderReviewHistory(box, rows); })
    .catch(function (e) { box.textContent = 'Failed to load history: ' + e.message; });
}

function renderReviewHistory(box, rows) {
  box.textContent = '';
  if (!Array.isArray(rows) || !rows.length) {
    box.textContent = 'No decisions recorded yet for this subject.';
    return;
  }
  rows.forEach(function (rev) {
    var line = document.createElement('div');
    var isCurrent = rev.status === 'decided' && !rev.superseded_by;
    line.className = 'review-history-row' + (isCurrent ? ' current' : '');

    var when = document.createElement('span');
    when.className = 'review-history-when';
    when.textContent = fmtReviewDate(rev.decided_at || rev.created_at);
    line.appendChild(when);

    var what = document.createElement('span');
    what.className = 'review-history-what';
    var outcome = rev.status;
    if (rev.chosen) outcome += ': ' + rev.chosen;
    if (isCurrent) outcome += ' (current)';
    what.textContent = '#' + rev.id + ' [' + rev.decision_type + '] ' + outcome;
    what.title = 'subject: ' + rev.subject;
    line.appendChild(what);

    // Only a superseded decided revision is restorable — an open row is
    // answerable above, and the current head is already in force.
    if (rev.status === 'decided' && rev.superseded_by) {
      var btn = document.createElement('button');
      btn.className = 'btn-sm review-restore-btn';
      btn.textContent = 'Restore this answer';
      btn.title = 'Append a new revision carrying this answer and re-apply it';
      btn.onclick = function () { revertReview(rev.id, box); };
      line.appendChild(btn);
    }
    box.appendChild(line);
  });
}

/* Restore routes through POST /api/review/revert — the same shared
   core::decisions::revert_decision the CLI uses. On success the inbox and the
   chain are both stale → reload the list (the chain re-opens on demand). */
function revertReview(id, box) {
  fetch('/api/review/revert', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id: id }),
  })
    .then(function (r) { return r.json().then(function (d) { return { ok: r.ok, d: d }; }); })
    .then(function (res) {
      if (!res.ok) {
        toast(res.d.error || 'Failed to restore', 'error');
      } else {
        toast('Restored: ' + res.d.chosen, 'info');
        loadReview();
        loadReviewCount();
      }
    })
    .catch(function (e) { toast('Network error: ' + e.message, 'error'); });
}

function fmtReviewDate(ts) {
  if (!ts) return '—';
  // Unix seconds → YYYY-MM-DD (UTC): compact, sortable, locale-stable.
  return new Date(ts * 1000).toISOString().slice(0, 10);
}

/* Optimistic: the card leaves immediately; a failure reloads the list so the
   question reappears (e.g. it was already answered from another surface). */
function answerReview(id, chosen, card) {
  removeReviewCard(card);
  fetch('/api/review/answer', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id: id, chosen: chosen }),
  })
    .then(function (r) { return r.json().then(function (d) { return { ok: r.ok, d: d }; }); })
    .then(function (res) {
      if (!res.ok) {
        toast(res.d.error || 'Failed to record answer', 'error');
        loadReview();
      } else {
        toast('Recorded: ' + chosen, 'info');
      }
      loadReviewCount();
    })
    .catch(function (e) { toast('Network error: ' + e.message, 'error'); loadReview(); });
}

function dismissReview(id, card) {
  removeReviewCard(card);
  fetch('/api/review/dismiss', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id: id }),
  })
    .then(function (r) { return r.json().then(function (d) { return { ok: r.ok, d: d }; }); })
    .then(function (res) {
      if (!res.ok) {
        toast(res.d.error || 'Failed to dismiss', 'error');
        loadReview();
      } else {
        toast('Dismissed — returns only if the evidence changes', 'info');
      }
      loadReviewCount();
    })
    .catch(function (e) { toast('Network error: ' + e.message, 'error'); loadReview(); });
}

function removeReviewCard(card) {
  var list = card.parentElement;
  // The focused button is about to vanish with its card — keep keyboard focus
  // inside the drawer's logical flow instead of dropping it to <body>.
  var next = card.nextElementSibling || card.previousElementSibling;
  card.remove();
  if (list && !list.children.length) {
    renderReviewEmpty(list);
    if (!list.hasAttribute('tabindex')) list.setAttribute('tabindex', '-1');
    list.focus();
  } else if (next) {
    var btn = next.querySelector('button');
    if (btn) btn.focus();
  }
}

function middleTruncate(s, max) {
  if (!s || s.length <= max) return s;
  var head = Math.ceil((max - 1) * 0.4);
  var tail = max - 1 - head;
  return s.slice(0, head) + '…' + s.slice(s.length - tail);
}
