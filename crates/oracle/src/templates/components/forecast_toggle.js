// Show forecast row after HTMX has loaded the content
// Called from hx-on--after-request
window.showForecast = function showForecast(stationId) {
  var forecastRow = document.getElementById("forecast-row-" + stationId);
  var weatherRow = document.querySelector(
    "tr[data-station='" + stationId + "']",
  );

  if (!forecastRow) {
    return;
  }

  // Mark as loaded and show
  forecastRow.dataset.loaded = "true";
  forecastRow.style.display = "table-row";
  if (weatherRow) {
    weatherRow.classList.add("is-expanded");
  }
};

// Toggle forecast visibility only if already loaded
// Called from onclick - does nothing on first click (HTMX handles that)
window.toggleForecastIfLoaded = function toggleForecastIfLoaded(stationId) {
  var forecastRow = document.getElementById("forecast-row-" + stationId);
  var weatherRow = document.querySelector(
    "tr[data-station='" + stationId + "']",
  );

  if (!forecastRow) {
    return;
  }

  // Only toggle if already loaded (not first click)
  if (forecastRow.dataset.loaded !== "true") {
    return;
  }

  // Check if hidden - handle both inline style and computed style
  var computedDisplay = window.getComputedStyle(forecastRow).display;
  var isHidden = computedDisplay === "none";

  if (isHidden) {
    forecastRow.style.display = "table-row";
    if (weatherRow) {
      weatherRow.classList.add("is-expanded");
    }
  } else {
    forecastRow.style.display = "none";
    if (weatherRow) {
      weatherRow.classList.remove("is-expanded");
    }
  }
};

// Legacy function for backwards compatibility
window.toggleForecast = window.toggleForecastIfLoaded;

// Re-initialize after HTMX swaps
document.addEventListener("htmx:afterSwap", function (event) {
  // Convert times in newly loaded forecast content
  if (typeof convertToLocalTime === "function") {
    convertToLocalTime();
  }
});
