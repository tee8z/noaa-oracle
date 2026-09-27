// Appended to the htmx asset, before site.js can start a request.
// htmx can defer initialize() with setTimeout at readyState "interactive";
// a fast request can finish before htmx:before:process ever fires.
(function () {
  if (!window.trustedTypes) return;
  var policy = trustedTypes.createPolicy("htmx", {
    createHTML: function (html) {
      return html;
    },
  });
  // Keep script creation unavailable: swapped scripts must remain blocked.
  htmx.registerExtension("trusted-types", {
    init: function (api) {
      api.initSecurity(policy);
    },
  });
})();
