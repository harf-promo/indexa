'use strict';

/* ── First-run onboarding ──
   The index is "empty" when it has no roots. On an empty index we reveal the guided
   first-run steps in the Context panel (#welcome-empty) and land the user there instead
   of the Ask view (whose copy assumes context already exists). This is derived from live
   state every load — no localStorage flag — so it self-dismisses the moment a folder is
   added (index becomes non-empty) and never nags a populated index. */

/* Toggle the Context-panel welcome between the populated default and the empty-index
   guidance. Safe to call before either node exists (no-ops on missing nodes). */
function applyEmptyState(isEmpty) {
  const empty = document.getElementById('welcome-empty');
  const def = document.getElementById('welcome-default');
  if (empty) empty.hidden = !isEmpty;
  if (def) def.hidden = isEmpty;
}

/* Detect an empty index and, if so, switch to the Context view and show the guided
   steps. On a network error we leave the default (populated) behavior untouched so a
   transient blip never flashes onboarding at an established user. */
async function detectEmptyAndOnboard() {
  let isEmpty = false;
  try {
    const r = await fetch('/api/roots');
    // A store error returns a 500 whose body is the JSON object {error:…}, which parses
    // fine — so bail on !r.ok rather than trust the body, and treat anything that isn't a
    // genuine empty array as "not empty". Never flash onboarding at an established user.
    if (!r.ok) return;
    const roots = await r.json();
    isEmpty = Array.isArray(roots) && roots.length === 0;
  } catch (e) {
    return; // network/parse error → leave the populated-index default (init landed on Ask)
  }
  if (isEmpty) {
    applyEmptyState(true);
    switchTab('tree');
  }
}
