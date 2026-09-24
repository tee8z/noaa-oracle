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

// Refresh the first dashboard after learning an offset that its request lacked.
(function () {
  var offset = String(-new Date().getTimezoneOffset());
  var previous = document.cookie.split(";").map(function (cookie) { return cookie.trim(); })
    .find(function (cookie) { return cookie.indexOf("utc_offset=") === 0; });
  document.cookie = "utc_offset=" + offset + ";path=/;SameSite=Lax;max-age=86400";
  if (previous !== "utc_offset=" + offset) document.documentElement.dataset.localDayChanged = "true";
})();
