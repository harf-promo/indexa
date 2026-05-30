/* ── Model-fit "ask me first" popover ──
   Gates the model-loading job kinds (summarize, index) on a pre-flight memory-fit
   estimate (GET /api/jobs/estimate). When the configured models won't fit the live
   memory budget, the user chooses rather than the engine silently loading a model
   that thrashes/freezes the machine.

   modelFitGate(path) resolves to a query-string suffix to append to the job-start
   POST:
     ''                            → proceed with the configured models
     '&file_model=…&dir_model=……'  → proceed with the recommended (fitting) models
   or null when the user cancels. It fails OPEN (returns '') on any estimate error,
   so a transient hiccup never blocks a build. */

async function modelFitGate(path) {
  let est;
  try {
    const r = await fetch('/api/jobs/estimate?path=' + encodeURIComponent(path));
    if (!r.ok) return '';
    est = await r.json();
  } catch (e) {
    return '';
  }
  if (est.configured_fits) return ''; // fits → no popover, proceed as configured
  return await showModelFitPopover(est);
}

function showModelFitPopover(est) {
  return new Promise(function (resolve) {
    const gb = function (b) { return (b / (1024 * 1024 * 1024)).toFixed(1); };
    const recFits = est.recommended_fits === true && !!est.recommended_dir_model;
    const recParams = recFits
      ? '&file_model=' + encodeURIComponent(est.recommended_file_model || est.recommended_dir_model) +
        '&dir_model=' + encodeURIComponent(est.recommended_dir_model) +
        '&num_ctx=' + (est.num_ctx || 4096)
      : '';

    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay fit-overlay';
    overlay.style.display = 'flex';
    // Static structure only; all server-supplied values are set via textContent
    // below so nothing is interpolated into HTML.
    overlay.innerHTML =
      '<div class="modal fit-modal" role="dialog" aria-modal="true">' +
      '<h2 class="fit-title">⚠ This build may run low on memory</h2>' +
      '<p class="fit-reason"></p>' +
      '<div class="fit-meter"><span class="fit-need"></span><span class="fit-budget"></span></div>' +
      '<div class="modal-actions fit-actions">' +
      (recFits ? '<button class="modal-btn primary" data-act="rec"></button>' : '') +
      '<button class="modal-btn" data-act="anyway"></button>' +
      '<button class="modal-btn" data-act="cancel">Cancel</button>' +
      '</div></div>';

    overlay.querySelector('.fit-reason').textContent =
      est.reason || (est.configured_dir_model + ' may not fit the available memory budget.');
    overlay.querySelector('.fit-need').textContent = 'Needs ~' + gb(est.configured_peak_bytes) + ' GB';
    overlay.querySelector('.fit-budget').textContent = 'Budget ~' + gb(est.budget_bytes) + ' GB';
    if (recFits) {
      overlay.querySelector('[data-act="rec"]').textContent = 'Use ' + est.recommended_dir_model + ' (fits)';
    }
    overlay.querySelector('[data-act="anyway"]').textContent = 'Build anyway (' + est.configured_dir_model + ')';

    function close(result) {
      if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
      document.removeEventListener('keydown', onKey);
      resolve(result);
    }
    function onKey(e) { if (e.key === 'Escape') close(null); }
    overlay.addEventListener('click', function (e) {
      if (e.target === overlay) { close(null); return; }
      const act = e.target.getAttribute && e.target.getAttribute('data-act');
      if (act === 'rec') close(recParams);
      else if (act === 'anyway') close('');
      else if (act === 'cancel') close(null);
    });
    document.addEventListener('keydown', onKey);
    document.body.appendChild(overlay);
    const primary = overlay.querySelector('.modal-btn.primary') || overlay.querySelector('[data-act="cancel"]');
    if (primary) primary.focus();
  });
}
