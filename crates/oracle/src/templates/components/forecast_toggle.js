// Load forecast data for a station
// Called from onclick on weather row
window.loadForecast = function loadForecast(stationId) {
  var forecastRow = document.getElementById("forecast-row-" + stationId);
  var forecastContainer = document.getElementById("forecast-" + stationId);
  var weatherRow = document.querySelector(
    "tr[data-station='" + stationId + "']",
  );

  if (!forecastRow || !forecastContainer) {
    return;
  }

  // If already loaded, just toggle visibility
  if (forecastRow.dataset.loaded === "true") {
    var isHidden = window.getComputedStyle(forecastRow).display === "none";
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
    return;
  }

  // First time - fetch the forecast data
  fetch("/fragments/forecast/" + stationId)
    .then(function (response) {
      return response.text();
    })
    .then(function (html) {
      forecastContainer.innerHTML = html;
      forecastRow.dataset.loaded = "true";
      forecastRow.style.display = "table-row";
      if (weatherRow) {
        weatherRow.classList.add("is-expanded");
      }
    })
    .catch(function (error) {
      console.error("Failed to load forecast:", error);
    });
};

// Legacy function for backwards compatibility
window.showForecast = window.loadForecast;

// Toggle forecast inside a mobile weather card
window.toggleCardForecast = function toggleCardForecast(stationId) {
  var container = document.getElementById("card-forecast-" + stationId);
  var card = container && container.closest(".weather-card");

  if (!container) return;

  // If already loaded, just toggle
  if (container.dataset.loaded === "true") {
    var isHidden = container.style.display === "none";
    container.style.display = isHidden ? "block" : "none";
    if (card) card.classList.toggle("is-expanded", isHidden);
    return;
  }

  // First time - fetch forecast
  fetch("/fragments/forecast/" + stationId)
    .then(function (response) {
      return response.text();
    })
    .then(function (html) {
      container.innerHTML = html;
      container.dataset.loaded = "true";
      container.style.display = "block";
      if (card) card.classList.add("is-expanded");
      if (typeof convertToLocalTime === "function") {
        convertToLocalTime();
      }
    })
    .catch(function (error) {
      console.error("Failed to load forecast:", error);
    });
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
