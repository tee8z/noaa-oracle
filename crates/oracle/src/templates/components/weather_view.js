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

  // Clear the previous station while the comparison loads.
  const forecastGrid = popup.querySelector(".popup-forecast-grid");
  const loadingEl = popup.querySelector(".popup-loading");
  if (forecastGrid) {
    forecastGrid.querySelectorAll("[data-field]").forEach((el) => {
      el.textContent = "—";
    });
  }
  if (loadingEl) loadingEl.style.display = "block";
  popup.style.display = "block";

  positionStationPopup(marker, popup);

  // Reposition after rendering because comparison labels can wrap on phones.
  fetchStationForecast(stationId, popup).then(() => {
    if (currentPopupStation === stationId) positionStationPopup(marker, popup);
  });
};

function positionStationPopup(marker, popup) {
  const mapRect = document.querySelector(".map-wrapper").getBoundingClientRect();
  const markerRect = marker.getBoundingClientRect();
  const { width, height } = popup.getBoundingClientRect();
  const viewport = document.documentElement;
  const margin = 10;

  // The popup can extend beyond a narrow map, but stays within the viewport.
  const center = markerRect.left + markerRect.width / 2;
  const left = Math.max(
    margin + width / 2,
    Math.min(center, viewport.clientWidth - width / 2 - margin),
  );
  const above = markerRect.top - height - margin;
  const preferredTop = above >= margin
    ? above
    : markerRect.top + markerRect.height + margin;
  const top = Math.max(
    margin,
    Math.min(preferredTop, viewport.clientHeight - height - margin),
  );

  popup.style.transform = "translateX(-50%)";
  popup.style.left = `${left - mapRect.left}px`;
  popup.style.top = `${top - mapRect.top}px`;
}

// Fetch forecast data for popup
async function fetchStationForecast(stationId, popup) {
  const loadingEl = popup.querySelector(".popup-loading");

  if (loadingEl) loadingEl.style.display = "block";

  try {
    // The APIs aggregate UTC calendar days. Include all of yesterday, even
    // when the popup is opened late in the day or across a local DST change.
    const today = new Date();
    today.setUTCHours(0, 0, 0, 0);
    const yesterday = new Date(today);
    yesterday.setUTCDate(yesterday.getUTCDate() - 1);
    const tomorrow = new Date(today);
    tomorrow.setUTCDate(tomorrow.getUTCDate() + 1);
    const dayAfterTomorrow = new Date(today);
    dayAfterTomorrow.setUTCDate(dayAfterTomorrow.getUTCDate() + 2);

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
    if (currentPopupStation !== stationId) return;

    // API dates can include a midnight timestamp. Match calendar dates without
    // timezone conversion, accepting both date-only and timestamp responses.
    const forecastByDate = {};
    forecasts.forEach((f) => {
      forecastByDate[f.date.slice(0, 10)] = f;
    });

    const obsByDate = {};
    observations.forEach((o) => {
      if (o.date) obsByDate[o.date.slice(0, 10)] = o;
    });

    // Formatting helpers
    // Match Rust/scoring: exact halves round away from zero.
    const wholeDegrees = (value) => Math.sign(value) * Math.round(Math.abs(value));
    const formatTemp = (high, low) => {
      if (high == null && low == null) return null;
      const bound = (value) => value == null ? "—" : `${wholeDegrees(value)}°`;
      return `${bound(high)} / ${bound(low)}`;
    };
    const formatWind = (speed) =>
      speed != null ? `${Math.round(speed)} kt` : null;
    const formatChance = (chance) => (chance != null ? `${chance}%` : null);
    const formatAmount = (amount) =>
      amount != null ? `${amount.toFixed(2)}"` : null;
    const formatHumidity = (max, min) => {
      if (max != null && max === min) return `${max}%`;
      if (max != null && min != null) return `${min}-${max}%`;
      if (max != null) return `${max}%`;
      if (min != null) return `${min}%`;
      return null;
    };

    // Set a single data-field element's text
    const setValue = (field, value) => {
      const el = popup.querySelector(`[data-field="${field}"]`);
      if (el) el.textContent = value ?? "—";
    };

    // Source labels live in the markup. Tomorrow and precipitation chance
    // have forecast fields only; past and current days retain both readings.
    const setCell = (day, metric, obsVal, fcstVal) => {
      setValue(`${day}-${metric}-obs`, obsVal);
      setValue(`${day}-${metric}-fcst`, fcstVal);
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
    if (currentPopupStation !== stationId) return;
    console.error("Error fetching forecast:", err);
    // Show error state
    popup.querySelectorAll("[data-field]").forEach((el) => {
      el.textContent = "?";
    });
  } finally {
    if (loadingEl && currentPopupStation === stationId) {
      loadingEl.style.display = "none";
    }
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

// Restore the selected view after replacement and after HTMX settles attributes.
function restoreWeatherView(e) {
  // OuterHTML can detach the original target before this event bubbles.
  const target = e.detail?.target || e.target;
  if (
    e.target.id === "weather-table-container" ||
    e.target.closest?.("#weather-table-container") ||
    target.id === "weather-table-container" ||
    target.closest?.("#weather-table-container") ||
    e.detail?.elt?.id === "weather-table-container"
  ) {
    const savedView = localStorage.getItem("weatherView") || "map";
    const mapView = document.getElementById("weather-map-view");
    const tableView = document.getElementById("weather-table-view");

    if (mapView && tableView) {
      switchWeatherView(savedView);
    }
  }
}
document.addEventListener("htmx:afterSwap", restoreWeatherView);
document.addEventListener("htmx:afterSettle", restoreWeatherView);

// Persist stations to localStorage when adding via dropdown
document.addEventListener("htmx:afterRequest", function (e) {
  // Check if this was an add_station request
  if (
    e.detail.pathInfo &&
    e.detail.pathInfo.requestPath.includes("add_station=")
  ) {
    const refreshPath = document.getElementById("weather-table-container")?.getAttribute("hx-get");
    if (!refreshPath) return;
    const url = new URL(refreshPath, window.location.origin);
    const stations = url.searchParams.get("stations");
    if (stations) {
      localStorage.setItem("weatherStations", stations);
    }
  }
});

// Refreshes and station additions retain the selection independently of rows.
document.addEventListener("htmx:configRequest", function (event) {
  if (!event.detail.path) return;
  const request = new URL(event.detail.path, window.location.origin);
  if (request.pathname !== "/fragments/weather") return;
  const container = document.getElementById("weather-table-container");
  const refreshPath = container?.getAttribute("hx-get");
  if (!refreshPath) return;
  const current = new URL(refreshPath, window.location.origin);
  // Older or empty fragments can lack context; the initial dashboard URL
  // remains a useful fallback until the first complete fragment is returned.
  const parameters = !current.search && window.location.pathname === "/"
    ? new URLSearchParams(window.location.search)
    : current.searchParams;
  for (const key of ["stations", "start", "end"]) {
    // HTMX appends parameters to the path's existing query string.
    if (!request.searchParams.has(key) && parameters.has(key)) {
      event.detail.parameters[key] = parameters.get(key);
    }
  }
});
