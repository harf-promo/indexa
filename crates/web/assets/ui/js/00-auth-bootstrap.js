// LAN-mode auth bootstrap. NO-OP on localhost (the default), where the server issues no token.
// When the page is opened over LAN with `?token=…`, persist it and attach it to same-origin
// `/api/` requests — an `Authorization: Bearer` header for fetch, and a `?token=` query for
// EventSource (which cannot set headers). Dependency-free, runs before any API call.
(function () {
  "use strict";
  var pageUrl = new URL(location.href);
  var fromUrl = pageUrl.searchParams.get("token");
  if (pageUrl.searchParams.has("token")) {
    pageUrl.searchParams.delete("token");
    history.replaceState(history.state, "", pageUrl.pathname + pageUrl.search + pageUrl.hash);
  }
  try {
    if (fromUrl) localStorage.setItem("indexa_web_token", fromUrl);
  } catch (e) {}
  var tok = null;
  try {
    tok = localStorage.getItem("indexa_web_token");
  } catch (e) {}
  if (!tok) return; // localhost / no token → leave fetch + EventSource untouched

  var sameOriginApiUrl = function (u) {
    var raw = typeof u === "string" ? u : (u && (typeof u.href === "string" ? u.href : u.url));
    if (typeof raw !== "string") return null;
    try {
      var parsed = new URL(raw, location.href);
      return parsed.origin === location.origin && parsed.pathname.startsWith("/api/") ? parsed : null;
    } catch (e) {
      return null;
    }
  };

  var _fetch = window.fetch;
  window.fetch = function (input, init) {
    init = init || {};
    if (sameOriginApiUrl(input)) {
      var h = new Headers(init.headers || (input && input.headers) || {});
      if (!h.has("Authorization")) h.set("Authorization", "Bearer " + tok);
      init.headers = h;
    }
    return _fetch.call(this, input, init);
  };

  if (window.EventSource) {
    var _ES = window.EventSource;
    var Wrapped = function (url, cfg) {
      var parsed = sameOriginApiUrl(url);
      if (parsed && !parsed.searchParams.has("token")) {
        parsed.searchParams.set("token", tok);
        url = parsed.href;
      }
      return new _ES(url, cfg);
    };
    Wrapped.prototype = _ES.prototype;
    window.EventSource = Wrapped;
  }
})();
