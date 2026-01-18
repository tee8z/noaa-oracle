// Weather View Toggle and Map Interactions

// Current station for popup
let currentPopupStation = null;

// Switch between map and table views
window.switchWeatherView = function (view) {
  const mapView = document.getElementById("weather-map-view");
  const tableView = document.getElementById("weather-table-view");
  const tabs = document.querySelectorAll(".tabs li[data-view]");

  if (!mapView || !tableView) return;

  // Update tab active state
  tabs.forEach((tab) => {
    if (tab.dataset.view === view) {
      tab.classList.add("is-active");
    } else {
      tab.classList.remove("is-active");
    }
  });

  // Show/hide views
  if (view === "map") {
    mapView.style.display = "block";
    tableView.style.display = "none";
  } else {
    mapView.style.display = "none";
    tableView.style.display = "block";
  }

  // Persist preference
  localStorage.setItem("weatherView", view);
};

// Show station popup on marker click
window.showStationPopup = function (marker) {
  const popup = document.getElementById("station-popup");
  if (!popup) return;

  // Get data from marker
  const stationId = marker.dataset.stationId;
  const stationName = marker.dataset.stationName;
  const state = marker.dataset.state;
  const iata = marker.dataset.iata;

  // Store current station for forecast link
  currentPopupStation = stationId;

  // Populate popup header
  popup.querySelector(".popup-station-id").textContent = stationId;
  const iataEl = popup.querySelector(".popup-iata");
  if (iata) {
    iataEl.textContent = iata;
    iataEl.style.display = "inline-block";
  } else {
    iataEl.style.display = "none";
  }

  const nameText = [stationName, state].filter(Boolean).join(", ");
  popup.querySelector(".popup-name").textContent = nameText;

  // Reset forecast values to loading state
  const forecastGrid = popup.querySelector(".popup-forecast-grid");
  const loadingEl = popup.querySelector(".popup-loading");
  if (forecastGrid) {
    forecastGrid.querySelectorAll(".forecast-value").forEach((el) => {
      el.textContent = "-";
    });
  }

  // Position popup near marker
  const mapWrapper = document.querySelector(".map-wrapper");
  const mapRect = mapWrapper.getBoundingClientRect();
  const markerRect = marker.getBoundingClientRect();

  // Calculate position relative to map wrapper
  let left = markerRect.left - mapRect.left + markerRect.width / 2;
  let top = markerRect.top - mapRect.top - 10;

  // Adjust if popup would go off screen
  const popupWidth = 360;
  const popupHeight = 280;

  if (left + popupWidth / 2 > mapRect.width) {
    left = mapRect.width - popupWidth / 2 - 10;
  }
  if (left - popupWidth / 2 < 0) {
    left = popupWidth / 2 + 10;
  }

  // Position above marker, but below if too close to top
  if (top < popupHeight) {
    top = markerRect.top - mapRect.top + markerRect.height + 10;
    popup.style.transform = "translateX(-50%)";
  } else {
    top = top - popupHeight;
    popup.style.transform = "translateX(-50%)";
  }

  popup.style.left = `${left}px`;
  popup.style.top = `${top}px`;
  popup.style.display = "block";

  // Fetch forecast data for this station
  fetchStationForecast(stationId, popup);
};

// Fetch forecast data for popup
async function fetchStationForecast(stationId, popup) {
  const loadingEl = popup.querySelector(".popup-loading");

  if (loadingEl) loadingEl.style.display = "block";

  try {
    // Get dates for yesterday, today, tomorrow in UTC
    const today = new Date();
    const yesterday = new Date(today);
    yesterday.setDate(yesterday.getDate() - 1);
    const tomorrow = new Date(today);
    tomorrow.setDate(tomorrow.getDate() + 1);
    const dayAfterTomorrow = new Date(today);
    dayAfterTomorrow.setDate(dayAfterTomorrow.getDate() + 2);

    // Format dates as ISO strings for API
    const formatDateParam = (d) => d.toISOString();
    const formatDateKey = (d) => d.toISOString().split("T")[0];

    const yesterdayKey = formatDateKey(yesterday);
    const todayKey = formatDateKey(today);
    const tomorrowKey = formatDateKey(tomorrow);

    // Fetch forecasts and observations in parallel
    const startDate = formatDateParam(yesterday);
    const endDate = formatDateParam(dayAfterTomorrow);

    const [forecastRes, obsRes] = await Promise.all([
      fetch(
        `/stations/forecasts?station_ids=${stationId}&start=${encodeURIComponent(startDate)}&end=${encodeURIComponent(endDate)}`,
      ),
      fetch(
        `/stations/observations?station_ids=${stationId}&start=${encodeURIComponent(startDate)}&end=${encodeURIComponent(endDate)}`,
      ),
    ]);

    const forecasts = forecastRes.ok ? await forecastRes.json() : [];
    const observations = obsRes.ok ? await obsRes.json() : [];

    // Index forecasts and observations by date
    const forecastByDate = {};
    forecasts.forEach((f) => {
      forecastByDate[f.date] = f;
    });

    const obsByDate = {};
    observations.forEach((o) => {
      // Observations have start_time, extract date from it
      const date = o.date || o.start_time?.split("T")[0];
      if (date) obsByDate[date] = o;
    });

    // Helper to format temp display
    const formatTemp = (high, low) => {
      if (high != null && low != null) {
        return `${Math.round(high)}° / ${Math.round(low)}°`;
      } else if (high != null) {
        return `${Math.round(high)}°`;
      } else if (low != null) {
        return `${Math.round(low)}°`;
      }
      return null;
    };

    // Helper to format wind
    const formatWind = (speed) => {
      if (speed != null) {
        return `${Math.round(speed)} mph`;
      }
      return null;
    };

    // Helper to format precip
    const formatPrecip = (chance) => {
      if (chance != null) {
        return `${chance}%`;
      }
      return null;
    };

    // Update popup with data
    const setValue = (field, value) => {
      const el = popup.querySelector(`[data-field="${field}"]`);
      if (el) el.textContent = value || "-";
    };

    // Yesterday
    const yesterdayObs = obsByDate[yesterdayKey];
    const yesterdayForecast = forecastByDate[yesterdayKey];
    setValue(
      "yesterday-temp-actual",
      yesterdayObs
        ? formatTemp(yesterdayObs.temp_high, yesterdayObs.temp_low)
        : null,
    );
    setValue(
      "yesterday-temp-forecast",
      yesterdayForecast
        ? formatTemp(yesterdayForecast.temp_high, yesterdayForecast.temp_low)
        : null,
    );
    setValue(
      "yesterday-wind",
      yesterdayObs
        ? formatWind(yesterdayObs.wind_speed)
        : yesterdayForecast
          ? formatWind(yesterdayForecast.wind_speed)
          : null,
    );
    setValue(
      "yesterday-precip",
      yesterdayForecast ? formatPrecip(yesterdayForecast.precip_chance) : null,
    );

    // Today
    const todayObs = obsByDate[todayKey];
    const todayForecast = forecastByDate[todayKey];
    setValue(
      "today-temp-actual",
      todayObs ? formatTemp(todayObs.temp_high, todayObs.temp_low) : null,
    );
    setValue(
      "today-temp-forecast",
      todayForecast
        ? formatTemp(todayForecast.temp_high, todayForecast.temp_low)
        : null,
    );
    setValue(
      "today-wind",
      todayObs
        ? formatWind(todayObs.wind_speed)
        : todayForecast
          ? formatWind(todayForecast.wind_speed)
          : null,
    );
    setValue(
      "today-precip",
      todayForecast ? formatPrecip(todayForecast.precip_chance) : null,
    );

    // Tomorrow
    const tomorrowForecast = forecastByDate[tomorrowKey];
    setValue(
      "tomorrow-temp-forecast",
      tomorrowForecast
        ? formatTemp(tomorrowForecast.temp_high, tomorrowForecast.temp_low)
        : null,
    );
    setValue(
      "tomorrow-wind",
      tomorrowForecast ? formatWind(tomorrowForecast.wind_speed) : null,
    );
    setValue(
      "tomorrow-precip",
      tomorrowForecast ? formatPrecip(tomorrowForecast.precip_chance) : null,
    );
  } catch (err) {
    console.error("Error fetching forecast:", err);
    // Show error state
    popup.querySelectorAll("[data-field]").forEach((el) => {
      el.textContent = "?";
    });
  } finally {
    if (loadingEl) loadingEl.style.display = "none";
  }
}

// Hide station popup
window.hideStationPopup = function () {
  const popup = document.getElementById("station-popup");
  if (popup) {
    popup.style.display = "none";
  }
  currentPopupStation = null;
};

// Load forecast from popup
window.loadForecastFromPopup = function () {
  if (!currentPopupStation) return;

  const stationId = currentPopupStation;
  hideStationPopup();

  // Switch to table view first
  switchWeatherView("table");

  // Wait for DOM to update, then load and scroll to forecast
  setTimeout(() => {
    if (typeof loadForecast === "function") {
      loadForecast(stationId);

      // Scroll to the weather row after a brief delay for the forecast to load
      setTimeout(() => {
        const weatherRow = document.querySelector(
          `tr[data-station='${stationId}']`,
        );
        if (weatherRow) {
          weatherRow.scrollIntoView({ behavior: "smooth", block: "start" });
        }
      }, 150);
    }
  }, 50);
};

// Close popup when clicking outside
document.addEventListener("click", function (e) {
  const popup = document.getElementById("station-popup");
  if (!popup) return;

  // Check if click is on a marker or inside popup
  if (e.target.classList.contains("station-marker")) return;
  if (popup.contains(e.target)) return;

  hideStationPopup();
});

// Initialize view preference on page load
document.addEventListener("DOMContentLoaded", function () {
  const savedView = localStorage.getItem("weatherView") || "map";
  // Only switch if we have the views available
  const mapView = document.getElementById("weather-map-view");
  const tableView = document.getElementById("weather-table-view");

  if (mapView && tableView) {
    switchWeatherView(savedView);
  }
});

// Re-initialize after HTMX swaps
document.addEventListener("htmx:afterSwap", function (e) {
  // Check if the weather container was updated
  if (
    e.target.id === "weather-table-container" ||
    e.target.closest("#weather-table-container")
  ) {
    const savedView = localStorage.getItem("weatherView") || "map";
    const mapView = document.getElementById("weather-map-view");
    const tableView = document.getElementById("weather-table-view");

    if (mapView && tableView) {
      switchWeatherView(savedView);
    }
  }
});

// Persist stations to localStorage when adding via dropdown
document.addEventListener("htmx:afterRequest", function (e) {
  // Check if this was an add_station request
  if (
    e.detail.pathInfo &&
    e.detail.pathInfo.requestPath.includes("add_station=")
  ) {
    // Extract current stations from URL or data attributes
    const url = new URL(window.location.href);
    const stations = url.searchParams.get("stations");
    if (stations) {
      localStorage.setItem("weatherStations", stations);
    }
  }
});
