const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const script = path.resolve(__dirname, "../../crates/oracle/src/templates/components/local_time.js");

test("forecast calendar dates stay on their UTC day while report timestamps localize", () => {
  const day = { textContent: "", getAttribute: () => "2026-09-20" };
  const report = { textContent: "", getAttribute: () => "2026-09-20T00:53:00Z" };
  const document = {
    readyState: "complete",
    addEventListener() {},
    querySelectorAll(selector) {
      if (selector === ".calendar-date[data-date]") return [day];
      if (selector === ".local-time[data-utc]") return [report];
      return [];
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document });
  assert.equal(day.textContent, new Date("2026-09-20T00:00:00Z").toLocaleDateString(undefined, {
    weekday: "short", month: "short", day: "numeric", timeZone: "UTC",
  }));
  assert.equal(report.textContent, new Date("2026-09-20T00:53:00Z").toLocaleString(undefined, {
    year: "numeric", month: "short", day: "numeric", hour: "2-digit",
    minute: "2-digit", timeZoneName: "short",
  }));
});
