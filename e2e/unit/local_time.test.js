const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const script = path.resolve(
  __dirname,
  "../../crates/oracle/src/templates/components/time/local_time.js",
);

function load(elements, readyState) {
  const listeners = {};
  const document = {
    readyState,
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
    querySelectorAll(selector) {
      return elements[selector] || [];
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document, Date, isNaN });
  return { document, listeners };
}

function run(elements) {
  const { document, listeners } = load(elements, "loading");
  // htmx processes the whole page first.
  listeners["htmx:after:process"]({ target: document });
  return listeners;
}

function utcTime(text = "x") {
  return {
    title: "",
    textContent: text,
    dataset: {},
    getAttribute: () => "2026-09-20T00:53:00Z",
  };
}

const dayTime = { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" };
const local = new Date("2026-09-20T00:53:00Z").toLocaleString(undefined, dayTime);

test("UTC times from the server are rewritten into local time, keeping UTC in the tooltip", () => {
  const time = utcTime("Sep 20, 2026 00:53 UTC");
  const window = {
    title: "",
    textContent: "Sep 24, 11:44–11:54 UTC",
    dataset: { start: "2026-09-24T11:44:00Z", end: "2026-09-24T11:54:00Z" },
  };
  run({
    "time.local-time[datetime]": [time],
    ".local-window[data-start][data-end]": [window],
  });
  assert.equal(time.title, "Sep 20, 2026 00:53 UTC");
  assert.ok(time.textContent.startsWith(local));
  assert.equal(window.title, "Sep 24, 11:44–11:54 UTC");
  assert.ok(window.textContent.includes("–"));
});

test("htmx swaps are localized too, and only once", () => {
  const time = utcTime();
  const listeners = run({});
  const target = {
    querySelectorAll: (selector) => (selector === "time.local-time[datetime]" ? [time] : []),
  };
  listeners["htmx:after:process"]({ target });
  const first = time.textContent;
  time.textContent = "changed elsewhere";
  listeners["htmx:after:process"]({ target });
  assert.equal(time.textContent, "changed elsewhere");
  assert.notEqual(first, "x");
  assert.equal(time.title, "x");
});

test("the page is localized once parsed, even when htmx processed it before the script ran", () => {
  const time = utcTime("Sep 29, 2026 16:42 UTC");
  const { listeners } = load({ "time.local-time[datetime]": [time] }, "loading");
  assert.equal(time.textContent, "Sep 29, 2026 16:42 UTC");
  listeners.DOMContentLoaded();
  assert.ok(time.textContent.startsWith(local));
  assert.equal(time.title, "Sep 29, 2026 16:42 UTC");
});

test("a script run after parsing localizes the page straight away", () => {
  for (const readyState of ["interactive", "complete"]) {
    const time = utcTime("Sep 29, 2026 16:42 UTC");
    const { listeners } = load({ "time.local-time[datetime]": [time] }, readyState);
    assert.ok(time.textContent.startsWith(local));
    assert.equal(listeners.DOMContentLoaded, undefined);
  }
});

test("a node reached by both the page load and htmx is rewritten once", () => {
  const time = utcTime("Sep 29, 2026 16:42 UTC");
  const { document, listeners } = load({ "time.local-time[datetime]": [time] }, "loading");
  listeners.DOMContentLoaded();
  time.textContent = "already local";
  listeners["htmx:after:process"]({ target: document });
  assert.equal(time.textContent, "already local");
  assert.equal(time.title, "Sep 29, 2026 16:42 UTC");
});
