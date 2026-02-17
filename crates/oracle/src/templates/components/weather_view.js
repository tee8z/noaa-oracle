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
        `/stations/daily-observations?station_ids=${stationId}&start=${encodeURIComponent(startDate)}&end=${encodeURIComponent(endDate)}`,
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
      if (o.date) obsByDate[o.date] = o;
    });

    // Formatting helpers
    const formatTemp = (high, low) => {
      if (high != null && low != null)
        return `${Math.round(high)}° / ${Math.round(low)}°`;
      if (high != null) return `${Math.round(high)}°`;
      if (low != null) return `${Math.round(low)}°`;
      return null;
    };
    const formatWind = (speed) =>
      speed != null ? `${Math.round(speed)} mph` : null;
    const formatChance = (chance) => (chance != null ? `${chance}%` : null);
    const formatAmount = (amount) =>
      amount != null && amount > 0 ? `${amount.toFixed(2)}"` : null;
    const formatHumidity = (max, min) => {
      if (max != null && min != null) return `${min}-${max}%`;
      if (max != null) return `${max}%`;
      if (min != null) return `${min}%`;
      return null;
    };

    // Set a single data-field element's text
    const setValue = (field, value) => {
      const el = popup.querySelector(`[data-field="${field}"]`);
      if (el) el.textContent = value ?? "-";
    };

    // Set both obs and fcst values for a cell
    const setCell = (day, metric, obsVal, fcstVal) => {
      setValue(`${day}-${metric}-obs`, obsVal ?? "-");
      setValue(`${day}-${metric}-fcst`, fcstVal ? `fcst: ${fcstVal}` : "");
    };

    // Populate a full day column for all metrics
    const populateDay = (day, obs, fcst) => {
      // Temp
      const obsTemp = obs ? formatTemp(obs.temp_high, obs.temp_low) : null;
      const fcstTemp = fcst ? formatTemp(fcst.temp_high, fcst.temp_low) : null;
      setCell(day, "temp", obsTemp, fcstTemp);

      // Wind
      const obsWind = obs ? formatWind(obs.wind_speed) : null;
      const fcstWind = fcst ? formatWind(fcst.wind_speed) : null;
      setCell(day, "wind", obsWind, fcstWind);

      // Chance (forecast-only, observations don't have precip_chance)
      const fcstChance = fcst ? formatChance(fcst.precip_chance) : null;
      setCell(day, "chance", null, fcstChance);

      // Rain
      const obsRain = obs ? formatAmount(obs.rain_amt) : null;
      const fcstRain = fcst ? formatAmount(fcst.rain_amt) : null;
      setCell(day, "rain", obsRain, fcstRain);

      // Snow
      const obsSnow = obs ? formatAmount(obs.snow_amt) : null;
      const fcstSnow = fcst ? formatAmount(fcst.snow_amt) : null;
      setCell(day, "snow", obsSnow, fcstSnow);

      // Humidity (obs has single value, forecast has min/max)
      const obsHumidity = obs
        ? formatHumidity(obs.humidity, obs.humidity)
        : null;
      const fcstHumidity = fcst
        ? formatHumidity(fcst.humidity_max, fcst.humidity_min)
        : null;
      setCell(day, "humidity", obsHumidity, fcstHumidity);
    };

    const yesterdayObs = obsByDate[yesterdayKey];
    const yesterdayForecast = forecastByDate[yesterdayKey];
    const todayObs = obsByDate[todayKey];
    const todayForecast = forecastByDate[todayKey];
    const tomorrowForecast = forecastByDate[tomorrowKey];

    populateDay("yesterday", yesterdayObs, yesterdayForecast);
    populateDay("today", todayObs, todayForecast);
    populateDay("tomorrow", null, tomorrowForecast);
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
