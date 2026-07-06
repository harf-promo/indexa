// LAN-mode auth bootstrap. NO-OP on localhost (the default), where the server issues no token.
// When the page is opened over LAN with `?token=…`, persist it and attach it to same-origin
// `/api/` requests — an `Authorization: Bearer` header for fetch, and a `?token=` query for
// EventSource (which cannot set headers). Dependency-free, runs before any API call.
(function () {
  "use strict";
  try {
    var fromUrl = new URLSearchParams(location.search).get("token");
    if (fromUrl) localStorage.setItem("indexa_web_token", fromUrl);
  } catch (e) {}
  var tok = null;
  try {
    tok = localStorage.getItem("indexa_web_token");
  } catch (e) {}
  if (!tok) return; // localhost / no token → leave fetch + EventSource untouched

  var isApi = function (u) {
    return typeof u === "string" && u.indexOf("/api/") !== -1;
  };

  var _fetch = window.fetch;
  window.fetch = function (input, init) {
    init = init || {};
    var url = typeof input === "string" ? input : (input && input.url) || "";
    if (isApi(url)) {
      var h = new Headers(init.headers || {});
      if (!h.has("Authorization")) h.set("Authorization", "Bearer " + tok);
      init.headers = h;
    }
    return _fetch.call(this, input, init);
  };

  if (window.EventSource) {
    var _ES = window.EventSource;
    var Wrapped = function (url, cfg) {
      if (isApi(url) && url.indexOf("token=") === -1) {
        url += (url.indexOf("?") === -1 ? "?" : "&") + "token=" + encodeURIComponent(tok);
      }
      return new _ES(url, cfg);
    };
    Wrapped.prototype = _ES.prototype;
    window.EventSource = Wrapped;
  }
})();
