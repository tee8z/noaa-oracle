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

// Refresh weather when the request used yesterday's day or a different offset.
(function () {
  var now = new Date();
  var midnight = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  var offset = String(-midnight.getTimezoneOffset());
  var cookies = document.cookie.split(";").map(function (cookie) { return cookie.trim(); });
  var day = String(Math.floor(midnight.getTime() / 1000));
  var changed = cookies.indexOf("utc_offset=" + offset) === -1 || cookies.indexOf("local_midnight=" + day) === -1;
  document.cookie = "local_midnight=" + day + ";path=/;SameSite=Lax;max-age=90000";
  document.cookie = "utc_offset=" + offset + ";path=/;SameSite=Lax;max-age=86400";
  if (changed) document.documentElement.dataset.localDayChanged = "true";
})();
