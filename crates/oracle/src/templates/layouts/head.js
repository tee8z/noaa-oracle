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

// Calendar days follow the reader's offset; scripts disabled fall back to UTC.
document.cookie = "utc_offset=" + (-new Date().getTimezoneOffset()) + ";path=/;SameSite=Lax;max-age=86400";
