// The weather section refreshes itself every five minutes. Skip a refresh
// while the reader has a list row open or is typing a search, so it doesn't
// close what they are reading. A station opened from the map survives the
// refresh (hx-preserve), so the map keeps refreshing.
function weatherBusy(section) {
  return section.querySelector("details.wx-station[open], #weather-search:focus");
}

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
