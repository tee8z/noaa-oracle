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

// An interactive request takes precedence over an automatic section refresh.
// Requests for individual station details still run independently.
document.addEventListener("htmx:beforeRequest", function (event) {
  var source = event.target;
  var section = source.closest && source.closest("#weather-table-container");
  if (section && source !== section) htmx.trigger(section, "htmx:abort");
});

// The reader may start typing or open a station after a refresh was sent.
document.addEventListener("htmx:beforeSwap", function (event) {
  var source = event.detail.requestConfig.elt;
  if (source.id !== "weather-table-container") return;
  var section = document.getElementById("weather-table-container");
  if (section && section.querySelector("details.wx-station[open], #weather-search:focus, #map-station > *")) {
    event.detail.shouldSwap = false;
  }
});
