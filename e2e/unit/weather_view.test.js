const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = path.resolve(
  __dirname,
  "../../crates/oracle/src/templates/components/weather_view.js",
);
const targets = [source];
if (process.env.ORACLE_UI_BUNDLE) {
  // The build job runs the same click/render checks against the shipped asset.
  targets.push(path.resolve(process.env.ORACLE_UI_BUNDLE));
}

const now = "2026-09-19T16:08:32.488Z";
class FixedDate extends Date {
  constructor(...args) {
    super(...(args.length ? args : [now]));
  }
}

async function openStationPopup(script, forecasts, observations, status = 200) {
  const fields = new Map();
  for (const day of ["yesterday", "today", "tomorrow"]) {
    for (const metric of ["temp", "wind", "chance", "rain", "snow", "humidity"]) {
      for (const kind of ["obs", "fcst"]) {
        if (kind === "obs" && (day === "tomorrow" || metric === "chance")) continue;
        fields.set(`${day}-${metric}-${kind}`, { textContent: "" });
      }
    }
  }
  const loading = { style: { display: "none" } };
  const errorText = { textContent: "" };
  const error = {
    style: { display: "none" },
    querySelector: (selector) => (selector === ".popup-error-text" ? errorText : null),
  };
  const elements = new Map([
    [".popup-loading", loading],
    [".popup-error", error],
    [".popup-station-id", { textContent: "" }],
    [".popup-iata", { textContent: "", style: {} }],
    [".popup-name", { textContent: "" }],
    [".popup-forecast-grid", { querySelectorAll: () => [...fields.values()] }],
  ]);
  const popup = {
    style: { display: "none" },
    getBoundingClientRect: () => ({ width: 460, height: 440 }),
    querySelector(selector) {
      const field = selector.match(/^\[data-field="([^"]+)"\]$/);
      return field ? fields.get(field[1]) : elements.get(selector);
    },
    querySelectorAll: () => [...fields.values()],
  };
  const marker = {
    dataset: { stationId: "KJFK", stationName: "John F Kennedy International", state: "NY", iata: "JFK" },
    getBoundingClientRect: () => ({ left: 400, top: 120, width: 6, height: 6 }),
  };
  const requests = [];
  const errors = [];
  const document = {
    readyState: "loading",
    documentElement: { clientWidth: 1280, clientHeight: 800 },
    addEventListener() {},
    body: { addEventListener() {} },
    getElementById: (id) => (id === "station-popup" ? popup : null),
    querySelector: (selector) => selector === ".map-wrapper"
      ? { getBoundingClientRect: () => ({ left: 0, top: 0, width: 640, height: 360 }) }
      : null,
    querySelectorAll: () => [],
  };
  const context = vm.createContext({
    Date: FixedDate,
    document,
    window: { addEventListener() {} },
    localStorage: { getItem: () => null, setItem() {} },
    console: { error: (...args) => errors.push(args), warn() {}, log() {} },
    fetch: async (url) => {
      requests.push(url);
      const endpoint = new URL(url, "https://4casttruth.win");
      assert.equal(endpoint.searchParams.get("station_ids"), "KJFK");
      assert.equal(endpoint.searchParams.get("start"), "2026-09-18T00:00:00.000Z");
      assert.equal(endpoint.searchParams.get("end"), "2026-09-21T00:00:00.000Z");
      const data = endpoint.pathname === "/stations/forecasts" ? forecasts
        : endpoint.pathname === "/stations/daily-observations" ? observations
        : assert.fail(`Unexpected popup request: ${endpoint.pathname}`);
      return { ok: status === 200, status, json: async () => data };
    },
  });
  vm.runInContext(fs.readFileSync(script, "utf8"), context, { filename: script });
  // Exercise the actual marker click entrypoint, including its fetch and render.
  context.window.showStationPopup(marker);
  await new Promise(setImmediate);
  assert.equal(popup.style.display, "block");
  assert.equal(loading.style.display, "none");
  assert.equal(elements.get(".popup-station-id").textContent, "KJFK");
  assert.equal(requests.length, 2);
  assert.deepEqual(errors, []);
  const field = (name) => fields.get(name)?.textContent;
  field.error = () => (error.style.display === "block" ? errorText.textContent : null);
  return field;
}

function weatherRequests(script, refreshPath, location) {
  const listeners = new Map();
  const views = {
    "weather-map-view": { style: { display: "block" } },
    "weather-table-view": { style: { display: "none" } },
  };
  const container = {
    id: "weather-table-container",
    getAttribute: (name) => name === "hx-get" ? refreshPath : null,
  };
  const storage = new Map([["weatherView", "table"]]);
  const document = {
    readyState: "loading",
    addEventListener(type, listener) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(listener);
    },
    body: { addEventListener() {} },
    getElementById: (id) => id === container.id ? container : views[id] ?? null,
    querySelector: () => null,
    querySelectorAll: () => [],
  };
  const context = vm.createContext({
    document,
    location: new URL(location),
    URL,
    URLSearchParams,
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value),
    },
    addEventListener() {},
    console,
  });
  // Browser window properties are global bindings, including switchWeatherView.
  context.window = context;
  vm.runInContext(fs.readFileSync(script, "utf8"), context, { filename: script });
  const dispatch = (type, detail = {}, target = container) => {
    for (const listener of listeners.get(type) ?? []) {
      listener({ target, detail });
    }
  };
  return {
    request(path, parameters = {}) {
      dispatch("htmx:configRequest", { path, parameters });
      return parameters;
    },
    swap: () => dispatch("htmx:afterSwap"),
    swapFromParent: () => dispatch("htmx:afterSwap", { target: container }, { id: "", closest: () => null }),
    settle: () => dispatch("htmx:afterSettle"),
    views,
  };
}

for (const script of targets) {
  test(`${path.basename(script)}: adding a station preserves the displayed stations and UTC range`, () => {
    const ui = weatherRequests(script,
      "/fragments/weather?stations=KPWM%2CKBOS&start=2026-09-18T00%3A00%3A00Z&end=2026-09-19T00%3A00%3A00Z",
      "https://4casttruth.win/?stations=KORD&start=2026-08-01T00:00:00Z&end=2026-08-02T00:00:00Z");
    assert.deepEqual(ui.request("/fragments/weather?add_station=KJFK", { add_station: "KJFK" }), {
      add_station: "KJFK",
      stations: "KPWM,KBOS",
      start: "2026-09-18T00:00:00Z",
      end: "2026-09-19T00:00:00Z",
    });
  });

  test(`${path.basename(script)}: an empty dashboard refresh retains selected stations and dates`, () => {
    const ui = weatherRequests(script, "/fragments/weather",
      "https://4casttruth.win/?stations=KPWM%2CKBOS&start=2026-09-18T00%3A00%3A00%2B02%3A00&end=2026-09-19T00%3A00%3A00%2B02%3A00");
    assert.deepEqual(ui.request("/fragments/weather"), {
      stations: "KPWM,KBOS",
      start: "2026-09-18T00:00:00+02:00",
      end: "2026-09-19T00:00:00+02:00",
    });
  });

  test(`${path.basename(script)}: adding a station to an empty dashboard retains its selection`, () => {
    const ui = weatherRequests(script, "/fragments/weather",
      "https://4casttruth.win/?stations=KPWM&start=2026-09-18T00:00:00Z&end=2026-09-19T00:00:00Z");
    assert.deepEqual(ui.request("/fragments/weather?add_station=KBOS", { add_station: "KBOS" }), {
      add_station: "KBOS",
      stations: "KPWM",
      start: "2026-09-18T00:00:00Z",
      end: "2026-09-19T00:00:00Z",
    });
  });

  test(`${path.basename(script)}: weather selections do not alter unrelated fragment requests`, () => {
    const ui = weatherRequests(script, "/fragments/weather?stations=KPWM",
      "https://4casttruth.win/?stations=KPWM&start=2026-09-18T00:00:00Z&end=2026-09-19T00:00:00Z");
    assert.deepEqual(ui.request("/fragments/event-stats", { filter: "live" }), { filter: "live" });
  });

  test(`${path.basename(script)}: refresh URLs do not receive duplicate selection parameters`, () => {
    const refresh = "/fragments/weather?stations=KPWM&start=2026-09-18T00:00:00Z&end=2026-09-19T00:00:00Z";
    const ui = weatherRequests(script, refresh, "https://4casttruth.win/");
    assert.deepEqual(ui.request(refresh), {});
  });

  test(`${path.basename(script)}: replacing the weather container restores the selected List view`, () => {
    const ui = weatherRequests(script, "/fragments/weather?stations=KPWM", "https://4casttruth.win/");
    ui.swap();
    assert.equal(ui.views["weather-map-view"].style.display, "none");
    assert.equal(ui.views["weather-table-view"].style.display, "block");
  });

  test(`${path.basename(script)}: an outer swap reported on the parent preserves List view`, () => {
    const ui = weatherRequests(script, "/fragments/weather?stations=KPWM", "https://4casttruth.win/");
    ui.swapFromParent();
    assert.equal(ui.views["weather-map-view"].style.display, "none");
    assert.equal(ui.views["weather-table-view"].style.display, "block");
  });

  test(`${path.basename(script)}: List view survives HTMX restoring incoming styles during settlement`, () => {
    const ui = weatherRequests(script, "/fragments/weather?stations=KPWM", "https://4casttruth.win/");
    ui.swap();
    // HTMX restores the new fragment's default attributes after afterSwap.
    ui.views["weather-map-view"].style.display = "block";
    ui.views["weather-table-view"].style.display = "none";
    ui.settle();
    assert.equal(ui.views["weather-map-view"].style.display, "none");
    assert.equal(ui.views["weather-table-view"].style.display, "block");
  });

  test(`${path.basename(script)}: renders SQL timestamp forecasts and observations from the popup APIs`, async () => {
    // These are the date strings and values returned by the live APIs when the
    // bug was reproduced. A raw timestamp key never matches the popup's day.
    const field = await openStationPopup(script, [
      { date: "2026-09-18 00:00:00", temp_high: 82, temp_low: 60, wind_speed: 11 },
      { date: "2026-09-19 00:00:00", temp_high: 82, temp_low: 60, wind_speed: 15, precip_chance: 1, humidity_max: 65, humidity_min: 40 },
      { date: "2026-09-20 00:00:00", temp_high: 73, temp_low: 60, wind_speed: 20, precip_chance: 81, rain_amt: 0.3 },
    ], [
      { date: "2026-09-18 00:00:00", temp_high: 82.94, temp_low: 75.02, wind_speed: 16 },
      { date: "2026-09-19 00:00:00", temp_high: 73.04, temp_low: 64.04, wind_speed: 13, humidity: 57 },
    ]);
    assert.equal(field("yesterday-temp-obs"), "83° / 75°");
    assert.equal(field("yesterday-temp-fcst"), "82° / 60°");
    assert.equal(field("today-temp-obs"), "73° / 64°");
    assert.equal(field("today-temp-fcst"), "82° / 60°");
    assert.equal(field("today-wind-obs"), "13 kt");
    assert.equal(field("today-wind-fcst"), "15 kt");
    assert.equal(field("today-humidity-obs"), "57%");
    assert.equal(field("today-humidity-fcst"), "40-65%");
    assert.equal(field("tomorrow-temp-obs"), undefined);
    assert.equal(field("tomorrow-temp-fcst"), "73° / 60°");
    assert.equal(field("tomorrow-chance-fcst"), "81%");
    assert.equal(field("tomorrow-rain-fcst"), '0.30"');
  });

  test(`${path.basename(script)}: keeps zero measurements distinct from missing comparison data`, async () => {
    const field = await openStationPopup(script, [
      { date: "2026-09-19", rain_amt: 0, snow_amt: 0, precip_chance: 0, humidity_min: 55, humidity_max: 55 },
      { date: "2026-09-20", rain_amt: 0, snow_amt: null },
    ], [
      { date: "2026-09-19", rain_amt: 0, snow_amt: null, humidity: 55, wind_speed: 0 },
    ]);
    assert.equal(field("today-rain-obs"), '0.00"');
    assert.equal(field("today-rain-fcst"), '0.00"');
    assert.equal(field("today-snow-obs"), "—");
    assert.equal(field("today-snow-fcst"), '0.00"');
    assert.equal(field("today-chance-obs"), undefined);
    assert.equal(field("today-chance-fcst"), "0%");
    assert.equal(field("today-humidity-obs"), "55%");
    assert.equal(field("today-humidity-fcst"), "55%");
    assert.equal(field("today-wind-obs"), "0 kt");
    assert.equal(field("today-wind-fcst"), "—");
    assert.equal(field("tomorrow-rain-fcst"), '0.00"');
    assert.equal(field("tomorrow-snow-fcst"), "—");
  });

  test(`${path.basename(script)}: partial temperatures retain high and low positions and half degrees round consistently`, async () => {
    const field = await openStationPopup(script, [
      { date: "2026-09-19", temp_high: null, temp_low: -2.5 },
      { date: "2026-09-20", temp_high: 54.5, temp_low: null },
    ], [
      { date: "2026-09-19", temp_high: 54.5, temp_low: -2.5 },
    ]);
    assert.equal(field("today-temp-obs"), "55° / -3°");
    assert.equal(field("today-temp-fcst"), "— / -3°");
    assert.equal(field("tomorrow-temp-fcst"), "55° / —");
  });

  test(`${path.basename(script)}: accepts date-only API values`, async () => {
    const field = await openStationPopup(script,
      [{ date: "2026-09-19", temp_high: 72, temp_low: 54 }],
      [{ date: "2026-09-19", temp_high: 68, temp_low: 55 }]);
    assert.equal(field("today-temp-fcst"), "72° / 54°");
    assert.equal(field("today-temp-obs"), "68° / 55°");
    assert.equal(field("yesterday-temp-obs"), "—");
    assert.equal(field("tomorrow-temp-fcst"), "—");
  });

  test(`${path.basename(script)}: keeps the API calendar day for ISO timestamps without timezone shifts`, async () => {
    const field = await openStationPopup(script,
      [{ date: "2026-09-20T00:00:00+10:00", temp_high: 74, temp_low: 56 }],
      [{ date: "2026-09-19T00:00:00Z", temp_high: 69, temp_low: 57 }]);
    assert.equal(field("tomorrow-temp-fcst"), "74° / 56°");
    assert.equal(field("today-temp-obs"), "69° / 57°");
    assert.equal(field("today-temp-fcst"), "—");
  });

  test(`${path.basename(script)}: empty API responses finish loading with empty cells`, async () => {
    const field = await openStationPopup(script, [], []);
    for (const day of ["yesterday", "today", "tomorrow"]) {
      assert.equal(field(`${day}-temp-obs`), day === "tomorrow" ? undefined : "—");
      assert.equal(field(`${day}-temp-fcst`), "—");
    }
    assert.equal(field.error(), null);
  });

  test(`${path.basename(script)}: failed API requests stop loading and say so`, async () => {
    const field = await openStationPopup(script, [], [], 503);
    assert.equal(field.error(), "The station data could not be loaded.");
    assert.equal(field("today-temp-obs"), "—");
  });
}
