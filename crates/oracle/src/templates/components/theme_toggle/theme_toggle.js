// Switch between light and dark and remember the choice. head.js applies
// the saved theme before the page paints.
document.addEventListener("click", function (event) {
  if (!event.target.closest("#theme-toggle")) return;
  var root = document.documentElement;
  var theme = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
  root.setAttribute("data-theme", theme);
  try {
    localStorage.setItem("theme", theme);
  } catch (error) {
    // Storage can be blocked or full; the switch still applies to this page.
  }
});
