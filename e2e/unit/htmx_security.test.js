const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.resolve(
  __dirname, "../../crates/oracle/src/templates/layouts/htmx_security.js",
), "utf8");

test("Trusted Types is installed before htmx processes the document or requests return", () => {
  let installed;
  const trustedTypes = {
    createPolicy(name, rules) {
      assert.equal(name, "htmx");
      return rules;
    },
  };
  vm.runInNewContext(source, {
    window: { trustedTypes }, trustedTypes,
    htmx: {
      registerExtension(name, extension) {
        assert.equal(name, "trusted-types");
        extension.init({ initSecurity(policy) { installed = policy; } });
      },
    },
  });
  assert.ok(installed, "registration must not wait for an event or timer");
  assert.equal(installed.createHTML("<p>Forecast</p>"), "<p>Forecast</p>");
  assert.equal(installed.createScript, undefined);
  assert.equal(installed.createScriptURL, undefined);
});

test("browsers without Trusted Types continue to use the existing CSP", () => {
  vm.runInNewContext(source, { window: {} });
});
