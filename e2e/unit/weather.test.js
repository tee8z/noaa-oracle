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
  const document = {
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
  };
  const htmx = {
    trigger(element, name) {
      triggered.push([element, name]);
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document, htmx });
  return { listeners, triggered };
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

test("interaction wins over an automatic refresh already in flight", () => {
  const { listeners, document, triggered } = load();
  const section = { id: "weather-table-container", querySelector: () => ({}) };
  document.getElementById = () => section;
  listeners["htmx:beforeRequest"]({ target: { closest: () => section } });
  assert.deepEqual(triggered, [[section, "htmx:abort"]]);
  const detail = { requestConfig: { elt: section }, shouldSwap: true };
  listeners["htmx:beforeSwap"]({ detail });
  assert.equal(detail.shouldSwap, false);
  const search = { requestConfig: { elt: { id: "weather-search" } }, shouldSwap: true };
  listeners["htmx:beforeSwap"]({ detail: search });
  assert.equal(search.shouldSwap, true);
});
