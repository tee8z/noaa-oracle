// Toggle forecast row visibility when weather row is clicked
// Attach to window to make it accessible from onclick handlers
window.toggleForecast = function toggleForecast(stationId) {
  var forecastRow = document.getElementById("forecast-row-" + stationId);
  var weatherRow = document.querySelector(
    "tr[data-station='" + stationId + "']",
  );

  if (!forecastRow) {
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

// Re-initialize after HTMX swaps
document.addEventListener("htmx:afterSwap", function (event) {
  // Convert times in newly loaded forecast content
  if (typeof convertToLocalTime === "function") {
    convertToLocalTime();
  }
});
