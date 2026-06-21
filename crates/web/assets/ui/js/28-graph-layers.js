// 28-graph-layers.js — knowledge-graph overlay toggle (Track 3, v0.70).
//
// Adds an opt-in "Related by meaning" layer to the Map's call graph: meaning-similarity edges
// between the displayed files (server param `&layers=semantic`). Shares the bundle scope with
// 19-graph.js (`graphState`, `fetchGraph`, `currentGraphScope`) — no module system here.
//
// Default OFF: with the toggle unchecked `graphState.semanticLayer` is falsy, so `fetchGraph`
// sends no `layers` param and the response + render are byte-identical to the call-graph-only view.

// eslint-disable-next-line no-unused-vars
function toggleSemanticLayer(on) {
  graphState.semanticLayer = !!on;
  // Re-fetch the current view (preserve a locked focus so the overlay applies to the same nodes).
  fetchGraph(currentGraphScope(), graphState.focusId || undefined, graphState.depth || 1);
}
