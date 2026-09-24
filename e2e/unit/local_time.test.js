const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const script = path.resolve(
  __dirname,
  "../../crates/oracle/src/templates/components/time/local_time.js",
);

function run(elements) {
  const listeners = {};
  const document = {
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
    querySelectorAll(selector) {
      return elements[selector] || [];
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document, Date, isNaN });
  listeners.DOMContentLoaded();
  return listeners;
}

test("UTC times from the server are rewritten into local time, keeping UTC in the tooltip", () => {
  const time = {
    title: "",
    textContent: "Sep 20, 2026 00:53 UTC",
    getAttribute: () => "2026-09-20T00:53:00Z",
  };
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
  assert.ok(
    time.textContent.startsWith(
      new Date("2026-09-20T00:53:00Z").toLocaleString(undefined, {
        month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
      }),
    ),
  );
  assert.equal(window.title, "Sep 24, 11:44–11:54 UTC");
  assert.ok(window.textContent.includes("–"));
});

test("htmx swaps are localized too, and only once", () => {
  const time = { title: "", textContent: "x", getAttribute: () => "2026-09-20T00:53:00Z" };
  const listeners = run({});
  const target = {
    querySelectorAll: (selector) => (selector === "time.local-time[datetime]" ? [time] : []),
  };
  listeners["htmx:load"]({ target });
  const first = time.textContent;
  listeners["htmx:load"]({ target });
  assert.equal(time.textContent, first);
  assert.equal(time.title, "x");
});
