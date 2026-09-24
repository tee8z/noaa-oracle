// Switch between light and dark and remember the choice. The layout's
// inline script applies the saved theme before the page paints.
document.addEventListener("click", function (event) {
  if (!event.target.closest("#theme-toggle")) return;
  var root = document.documentElement;
  var theme = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
  root.setAttribute("data-theme", theme);
  localStorage.setItem("theme", theme);
});
