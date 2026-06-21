// 28-graph-layers.js — knowledge-graph overlay toggles (Track 3, v0.70 semantic + v0.71 category).
//
// Adds opt-in overlays to the Map's call graph: "Related by meaning" (meaning-similarity edges,
// `&layers=semantic`) and "Same category" (shared-classification grouping, `&layers=category`).
// Shares the bundle scope with 19-graph.js (`graphState`, `fetchGraph`, `currentGraphScope`).
//
// Default OFF: with both toggles unchecked `fetchGraph` sends no `layers` param, so the response +
// render are byte-identical to the call-graph-only view.

function refetchGraphView() {
  // Re-fetch the current view (preserve a locked focus so overlays apply to the same nodes).
  fetchGraph(currentGraphScope(), graphState.focusId || undefined, graphState.depth || 1);
}

// eslint-disable-next-line no-unused-vars
function toggleSemanticLayer(on) {
  graphState.semanticLayer = !!on;
  refetchGraphView();
}

// eslint-disable-next-line no-unused-vars
function toggleCategoryLayer(on) {
  graphState.categoryLayer = !!on;
  refetchGraphView();
}

// eslint-disable-next-line no-unused-vars
function togglePackLayer(on) {
  graphState.packLayer = !!on;
  refetchGraphView();
}
