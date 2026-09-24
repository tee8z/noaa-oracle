// Runs in <head>, before the page paints: apply the saved theme, else the
// system's, so a dark page never flashes light.
(function () {
  var theme = null;
  try {
    theme = localStorage.getItem("theme");
  } catch (error) {
    // Storage can be blocked; fall back to the system preference.
  }
  if (theme !== "dark" && theme !== "light") {
    theme = matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  document.documentElement.setAttribute("data-theme", theme);
})();

// Pages require Trusted Types (policy.rs): only the "htmx" policy may turn a
// string into HTML, and htmx parses every response it swaps through it. The
// policy has no createScript, so a <script> in a swapped response throws
// instead of running. htmx fires this event before it first reads the page,
// so no response is parsed without the policy.
document.addEventListener(
  "htmx:before:process",
  function () {
    if (!window.trustedTypes) return;
    var policy = trustedTypes.createPolicy("htmx", {
      createHTML: function (html) {
        return html;
      },
    });
    htmx.registerExtension("trusted-types", {
      init: function (api) {
        api.initSecurity(policy);
      },
    });
  },
  { once: true },
);
