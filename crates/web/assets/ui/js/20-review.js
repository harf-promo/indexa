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
  card.appendChild(row);
  return card;
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
