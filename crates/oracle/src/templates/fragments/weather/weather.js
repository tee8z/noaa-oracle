// The weather section refreshes itself every five minutes. Skip a refresh
// while the reader has a list row open or is typing a search, so it doesn't
// close what they are reading. A station opened from the map survives the
// refresh (hx-preserve), so the map keeps refreshing.
function weatherBusy(section) {
  return section.querySelector("details.wx-station[open], #weather-search:focus");
}

// Read the loaded panel, rather than the last clicked pin: a failed or
// superseded request must not select a station whose data never arrived.
// The preserved panel also restores the selection after a map refresh.
function syncMapSelection() {
  var panel = document.getElementById("map-station");
  if (!panel) return;
  var heading = panel.querySelector(".station-detail-head h3");
  var station = heading && heading.textContent.trim();
  document.querySelectorAll(".station-markers .pin").forEach(function (pin) {
    if (pin.dataset.station === station) pin.setAttribute("aria-current", "true");
    else pin.removeAttribute("aria-current");
  });
}

document.addEventListener("htmx:after:process", syncMapSelection);
// Load errors replace the panel directly rather than processing a fragment.
document.addEventListener("htmx:after:request", syncMapSelection);
document.addEventListener("htmx:error", syncMapSelection);

// Enter scrolls the selected station into view. Move keyboard focus with it
// so the next Tab reaches its links instead of every off-screen map pin.
// A background refresh, or a reader who has already moved on, keeps focus.
document.addEventListener("htmx:after:swap", function (event) {
  var ctx = event.detail.ctx;
  if (!ctx || !ctx.target || ctx.target.id !== "map-station") return;
  var source = ctx.sourceElement;
  if (!source || !source.contains(document.activeElement)) return;
  var heading = ctx.target.querySelector(".station-detail-head h3");
  if (!heading) return;
  heading.tabIndex = -1;
  heading.focus({ preventScroll: true });
});

document.addEventListener("htmx:config:request", function (event) {
  var section = event.target;
  if (section.id === "weather-table-container" && weatherBusy(section)) {
    event.preventDefault();
  }
});

// The reader may start typing or open a station after a refresh was sent.
document.addEventListener("htmx:before:swap", function (event) {
  if (event.detail.ctx.sourceElement.id !== "weather-table-container") return;
  var section = document.getElementById("weather-table-container");
  if (section && weatherBusy(section)) event.preventDefault();
});

// The page arrived without the reader's time zone (see head.js), so its
// weather shows UTC days. Fetch it once more now that the cookie is set.
// Explicit periods are UTC days either way.
document.addEventListener("DOMContentLoaded", function () {
  if (!document.documentElement.dataset.zoneChanged) return;
  delete document.documentElement.dataset.zoneChanged;
  var section = document.getElementById("weather-table-container");
  var path = section && section.getAttribute("hx-get");
  if (path && !/[?&](start|end)=/.test(path)) {
    htmx.ajax("GET", path, { source: section, target: section, swap: "outerHTML" });
  }
});
