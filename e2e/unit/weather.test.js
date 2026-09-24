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
  const triggered = [];
  const requested = [];
  const document = {
    documentElement: { dataset: {} },
    getElementById() { return null; },
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
  };
  const htmx = {
    ajax(method, url, options) { requested.push([method, url, options]); },
    trigger(element, name) {
      triggered.push([element, name]);
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document, htmx });
  return { listeners, triggered, requested, document };
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
  listeners["htmx:confirm"](skipped);
  assert.equal(skipped.prevented, true);

  const refreshed = confirmEvent(idle);
  listeners["htmx:confirm"](refreshed);
  assert.equal(refreshed.prevented, false);

  // Tabs and links inside the section still work.
  const clicked = confirmEvent(tab);
  listeners["htmx:confirm"](clicked);
  assert.equal(clicked.prevented, false);
});

test("Enter or Space on a map pin opens it like a click", () => {
  const { listeners, triggered } = load();
  const pin = {};
  const target = { closest: (selector) => (selector === ".pin[hx-get]" ? pin : null) };
  for (const key of ["Enter", " ", "a"]) {
    let prevented = false;
    listeners.keydown({ key, target, preventDefault: () => (prevented = true) });
    assert.equal(prevented, key !== "a", key);
  }
  assert.deepEqual(triggered, [
    [pin, "click"],
    [pin, "click"],
  ]);
  listeners.keydown({ key: "Enter", target: { closest: () => null }, preventDefault() {} });
  assert.equal(triggered.length, 2);
});

test("a new browser offset refreshes weather once and keeps the selected view", () => {
  const { listeners, document, requested } = load();
  const section = { getAttribute: () => "/fragments/weather?view=list&stations=KORD" };
  document.getElementById = () => section;
  listeners.DOMContentLoaded();
  assert.equal(requested.length, 0);
  document.documentElement.dataset.localDayChanged = "true";
  listeners.DOMContentLoaded();
  assert.equal(requested.length, 1);
  assert.equal(requested[0][0], "GET");
  assert.equal(requested[0][1], "/fragments/weather?view=list&stations=KORD");
  assert.equal(requested[0][2].target, section);
  assert.equal(requested[0][2].swap, "outerHTML");
  listeners.DOMContentLoaded();
  assert.equal(requested.length, 1);
});
