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

async function openStationPopup(script, forecasts, observations) {
  const fields = new Map();
  for (const day of ["yesterday", "today", "tomorrow"]) {
    for (const metric of ["temp", "wind", "chance", "rain", "snow", "humidity"]) {
      for (const kind of ["obs", "fcst"]) {
        fields.set(`${day}-${metric}-${kind}`, { textContent: "" });
      }
    }
  }
  const loading = { style: { display: "none" } };
  const elements = new Map([
    [".popup-loading", loading],
    [".popup-station-id", { textContent: "" }],
    [".popup-iata", { textContent: "", style: {} }],
    [".popup-name", { textContent: "" }],
    [".popup-forecast-grid", { querySelectorAll: () => [] }],
  ]);
  const popup = {
    style: { display: "none" },
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
      assert.equal(endpoint.searchParams.get("start"), "2026-09-18T16:08:32.488Z");
      assert.equal(endpoint.searchParams.get("end"), "2026-09-21T16:08:32.488Z");
      const data = endpoint.pathname === "/stations/forecasts" ? forecasts
        : endpoint.pathname === "/stations/daily-observations" ? observations
        : assert.fail(`Unexpected popup request: ${endpoint.pathname}`);
      return { ok: true, json: async () => data };
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
  return (field) => fields.get(field).textContent;
}

for (const script of targets) {
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
    assert.equal(field("yesterday-temp-fcst"), "fcst: 82° / 60°");
    assert.equal(field("today-temp-obs"), "73° / 64°");
    assert.equal(field("today-temp-fcst"), "fcst: 82° / 60°");
    assert.equal(field("today-wind-obs"), "13 kt");
    assert.equal(field("today-wind-fcst"), "fcst: 15 kt");
    assert.equal(field("today-humidity-obs"), "57-57%");
    assert.equal(field("today-humidity-fcst"), "fcst: 40-65%");
    assert.equal(field("tomorrow-temp-obs"), "-");
    assert.equal(field("tomorrow-temp-fcst"), "fcst: 73° / 60°");
    assert.equal(field("tomorrow-chance-fcst"), "fcst: 81%");
    assert.equal(field("tomorrow-rain-fcst"), 'fcst: 0.30"');
  });

  test(`${path.basename(script)}: accepts date-only API values`, async () => {
    const field = await openStationPopup(script,
      [{ date: "2026-09-19", temp_high: 72, temp_low: 54 }],
      [{ date: "2026-09-19", temp_high: 68, temp_low: 55 }]);
    assert.equal(field("today-temp-fcst"), "fcst: 72° / 54°");
    assert.equal(field("today-temp-obs"), "68° / 55°");
    assert.equal(field("yesterday-temp-obs"), "-");
    assert.equal(field("tomorrow-temp-fcst"), "");
  });

  test(`${path.basename(script)}: keeps the API calendar day for ISO timestamps without timezone shifts`, async () => {
    const field = await openStationPopup(script,
      [{ date: "2026-09-20T00:00:00+10:00", temp_high: 74, temp_low: 56 }],
      [{ date: "2026-09-19T00:00:00Z", temp_high: 69, temp_low: 57 }]);
    assert.equal(field("tomorrow-temp-fcst"), "fcst: 74° / 56°");
    assert.equal(field("today-temp-obs"), "69° / 57°");
    assert.equal(field("today-temp-fcst"), "");
  });

  test(`${path.basename(script)}: empty API responses finish loading with empty cells`, async () => {
    const field = await openStationPopup(script, [], []);
    for (const day of ["yesterday", "today", "tomorrow"]) {
      assert.equal(field(`${day}-temp-obs`), "-");
      assert.equal(field(`${day}-temp-fcst`), "");
    }
  });
}
