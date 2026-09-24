// The weather section refreshes itself every five minutes. Skip a refresh
// while the reader has a station open or is typing a search, so it doesn't
// close what they are reading.
document.addEventListener("htmx:confirm", function (event) {
  var section = event.target;
  if (section.id !== "weather-table-container") return;
  if (section.querySelector("details.wx-station[open], #weather-search:focus, #map-station > *")) {
    event.preventDefault();
  }
});

// Map pins are SVG groups that act as buttons: Enter or Space opens one.
document.addEventListener("keydown", function (event) {
  if (event.key !== "Enter" && event.key !== " ") return;
  var pin = event.target.closest && event.target.closest(".pin[hx-get]");
  if (!pin) return;
  event.preventDefault();
  htmx.trigger(pin, "click");
});

// The initial document used UTC if it arrived without an offset cookie.
// Fetch its weather once after head.js has supplied the reader's offset.
document.addEventListener("DOMContentLoaded", function () {
  if (!document.documentElement.dataset.localDayChanged) return;
  delete document.documentElement.dataset.localDayChanged;
  var section = document.getElementById("weather-table-container");
  if (section) htmx.ajax("GET", section.getAttribute("hx-get"), { target: section, swap: "outerHTML" });
});
