const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const script = path.resolve(
  __dirname,
  "../../crates/oracle/src/templates/fragments/weather/weather.js",
);

function load() {
  const listeners = {};
  const document = {
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document });
  return { listeners, document };
}

function confirmEvent(target) {
  const event = { target, prevented: false };
  event.preventDefault = () => {
    event.prevented = true;
  };
  return event;
}

test("the five-minute refresh waits while a station is open or the search has focus", () => {
  const { listeners } = load();
  const busy = { id: "weather-table-container", querySelector: () => ({}) };
  const idle = { id: "weather-table-container", querySelector: () => null };
  const tab = { id: "", querySelector: () => ({}) };

  const skipped = confirmEvent(busy);
  listeners["htmx:config:request"](skipped);
  assert.equal(skipped.prevented, true);

  const refreshed = confirmEvent(idle);
  listeners["htmx:config:request"](refreshed);
  assert.equal(refreshed.prevented, false);

  // Tabs and links inside the section still work.
  const clicked = confirmEvent(tab);
  listeners["htmx:config:request"](clicked);
  assert.equal(clicked.prevented, false);
});

test("a refresh that returns while the reader is busy is not swapped in", () => {
  const { listeners, document } = load();
  const section = { id: "weather-table-container", querySelector: () => ({}) };
  document.getElementById = () => section;
  const refresh = confirmEvent(section);
  refresh.detail = { ctx: { sourceElement: section } };
  listeners["htmx:before:swap"](refresh);
  assert.equal(refresh.prevented, true);
  const search = confirmEvent({ id: "weather-search" });
  search.detail = { ctx: { sourceElement: { id: "weather-search" } } };
  listeners["htmx:before:swap"](search);
  assert.equal(search.prevented, false);
});

test("a page rendered before the time zone was known fetches its weather once more", () => {
  const listeners = {};
  const requests = [];
  const section = {
    id: "weather-table-container",
    getAttribute: () => "/fragments/weather?view=list",
  };
  const document = {
    documentElement: { dataset: { zoneChanged: "true" } },
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
    getElementById: () => section,
  };
  const htmx = { ajax: (...request) => requests.push(request) };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document, htmx });
  listeners["DOMContentLoaded"]();
  assert.equal(requests.length, 1);
  assert.deepEqual(requests[0].slice(0, 2), ["GET", "/fragments/weather?view=list"]);
  assert.equal(document.documentElement.dataset.zoneChanged, undefined);
  // A known zone, or an explicit period in UTC days: no second request.
  listeners["DOMContentLoaded"]();
  document.documentElement.dataset.zoneChanged = "true";
  section.getAttribute = () => "/fragments/weather?start=2026-09-20T00%3A00%3A00Z";
  listeners["DOMContentLoaded"]();
  assert.equal(requests.length, 1);
});
