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

// The server shows the reader's calendar days from their time zone in the
// `tz` cookie (see routes/ui/local_day.rs). Store it now, and again before
// each htmx request in case it changed while the page was open. On a first
// visit the page was rendered for UTC days; weather.js fetches its weather
// once more with the cookie.
(function () {
  function store() {
    var zone = "";
    try {
      zone = Intl.DateTimeFormat().resolvedOptions().timeZone || "";
    } catch (error) {
      // Without a zone the server keeps UTC days.
    }
    if (!/^[A-Za-z0-9_+\-\/]{1,64}$/.test(zone)) return false;
    if (document.cookie.split("; ").indexOf("tz=" + zone) !== -1) return false;
    document.cookie = "tz=" + zone + ";path=/;SameSite=Lax;max-age=31536000";
    return true;
  }
  if (store()) document.documentElement.dataset.zoneChanged = "true";
  document.addEventListener("htmx:config:request", store);
})();
