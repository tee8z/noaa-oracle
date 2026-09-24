const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const script = path.resolve(__dirname, "../../crates/oracle/src/templates/layouts/head.js");

// Runs head.js with a browser in `zone` and the cookies in `cookies`.
function load(zone, cookies) {
  const listeners = {};
  const jar = new Map(
    cookies ? cookies.split("; ").map((cookie) => cookie.split("=")) : [],
  );
  const document = {
    documentElement: { dataset: {}, setAttribute() {} },
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
    get cookie() {
      return [...jar].map(([name, value]) => `${name}=${value}`).join("; ");
    },
    set cookie(value) {
      const [pair] = value.split(";");
      const [name, ...rest] = pair.split("=");
      jar.set(name, rest.join("="));
    },
  };
  const context = {
    document,
    localStorage: { getItem: () => null },
    matchMedia: () => ({ matches: false }),
    Intl: {
      DateTimeFormat: () => ({ resolvedOptions: () => ({ timeZone: zone.current }) }),
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), context);
  return { listeners, document, jar };
}

test("the first visit stores the time zone and asks for the weather again", () => {
  const zone = { current: "America/New_York" };
  const { document, jar } = load(zone, "weather_view=map");
  assert.equal(jar.get("tz"), "America/New_York");
  assert.equal(jar.get("weather_view"), "map");
  assert.equal(document.documentElement.dataset.zoneChanged, "true");
});

test("a stored zone changes nothing", () => {
  const zone = { current: "America/New_York" };
  const { document } = load(zone, "tz=America/New_York");
  assert.equal(document.documentElement.dataset.zoneChanged, undefined);
});

test("htmx requests carry a zone that changed while the page was open", () => {
  const zone = { current: "America/New_York" };
  const { listeners, jar } = load(zone, "tz=America/New_York");
  zone.current = "America/Los_Angeles";
  listeners["htmx:config:request"]({});
  assert.equal(jar.get("tz"), "America/Los_Angeles");
});

test("a missing or odd zone leaves the cookie alone", () => {
  for (const current of ["", undefined, "Bad Zone; tz=x", "x".repeat(65)]) {
    const { document, jar } = load({ current }, "");
    assert.equal(jar.has("tz"), false, String(current));
    assert.equal(document.documentElement.dataset.zoneChanged, undefined);
  }
});
