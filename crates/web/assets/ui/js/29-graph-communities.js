// 29-graph-communities.js — the Map's opt-in "Communities" overlay (Track 3, v0.72).
//
// Louvain clustering of the call graph (computed server-side): tints nodes by community, emphasizes
// each community's hub, and highlights cross-community "bridge" edges (surprising connections).
// Shares the bundle scope with 19-graph.js (`graphState`, `fetchGraph`, `currentGraphScope`).
//
// Default OFF: with the toggle unchecked `fetchGraph` sends no `communities` layer, so the response
// + render are byte-identical to the plain call graph.

// eslint-disable-next-line no-unused-vars
function toggleCommunitiesLayer(on) {
  graphState.communitiesLayer = !!on;
  refetchGraphView();
}

// Number of distinct community colours before the rest merge into a neutral grey. Keeping it small
// (and low-saturation) is what stops the data layer becoming a rainbow that fights the Harf palette.
var COMMUNITY_COLOR_CAP = 6;

// A community tint by SIZE RANK (0 = largest). Low-saturation HSL, theme-aware lightness, applied
// inline on SVG circles only — the one sanctioned categorical-colour exception to the two-brand-colour
// rule (it's a data-viz encoding, never UI chrome; green/teal/info stay reserved for state). Ranks at
// or beyond the cap (and any unranked node) fall back to a neutral grey so the tail reads as "other".
// eslint-disable-next-line no-unused-vars
function communityTint(rank) {
  var dark =
    (document.documentElement.getAttribute('data-theme') || 'dark') !== 'light';
  if (rank == null || rank >= COMMUNITY_COLOR_CAP) {
    return dark ? '#6E7073' : '#939598'; // --ink-4 / --harf-grey neutral "other"
  }
  var hue = Math.round((rank * 360) / COMMUNITY_COLOR_CAP);
  var sat = 22; // low saturation: a tint, not a rainbow
  var light = dark ? 62 : 44;
  return 'hsl(' + hue + ', ' + sat + '%, ' + light + '%)';
}
