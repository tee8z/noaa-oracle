const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const script = path.resolve(
  __dirname,
  "../../crates/oracle/src/templates/components/load_error/load_error.js",
);

function element(tag) {
  return {
    tag,
    attributes: {},
    listeners: {},
    children: [],
    setAttribute(name, value) {
      this.attributes[name] = value;
    },
    addEventListener(name, listener) {
      this.listeners[name] = listener;
    },
    append(...children) {
      this.children.push(...children);
    },
  };
}

function load() {
  const listeners = {};
  const requests = [];
  const document = {
    addEventListener(name, listener) {
      listeners[name] = listener;
    },
    createElement: element,
  };
  const htmx = {
    ajax(verb, path, options) {
      requests.push([verb, path, options]);
    },
  };
  vm.runInNewContext(fs.readFileSync(script, "utf8"), { document, htmx, WeakMap });
  return { listeners, requests };
}

function target(message) {
  return {
    isConnected: true,
    content: null,
    hasAttribute: (name) => name === "data-load-error" && message !== undefined,
    getAttribute: () => message,
    replaceChildren(child) {
      this.content = child;
    },
  };
}

test("a request with no reply shows the target's message and a retry", () => {
  const { listeners, requests } = load();
  const panel = target("Couldn't load this station's forecast and history.");
  const pin = { isConnected: true };
  const ctx = { target: panel, sourceElement: pin, request: { action: "/fragments/station/KORD" } };
  listeners["htmx:before:request"]({ detail: { ctx } });
  listeners["htmx:error"]({ detail: { ctx } });

  const box = panel.content;
  assert.equal(box.className, "load-error");
  assert.equal(box.attributes.role, "alert");
  const [message, retry] = box.children;
  assert.equal(message.textContent, "Couldn't load this station's forecast and history.");
  assert.equal(retry.textContent, "Try again");
  retry.listeners.click();
  assert.equal(requests.length, 1);
  const [verb, url, options] = requests[0];
  assert.equal(verb, "GET");
  assert.equal(url, "/fragments/station/KORD");
  assert.equal(options.source, pin);
  assert.equal(options.target, panel);
  assert.equal(options.swap, "innerHTML");
});

test("a request replaced by a newer one, or an unmarked target, shows nothing", () => {
  const { listeners } = load();
  const panel = target("Couldn't load.");
  const first = { target: panel, request: { action: "/fragments/station/KORD" } };
  const second = { target: panel, request: { action: "/fragments/station/KBOS" } };
  listeners["htmx:before:request"]({ detail: { ctx: first } });
  listeners["htmx:before:request"]({ detail: { ctx: second } });
  listeners["htmx:error"]({ detail: { ctx: first } });
  assert.equal(panel.content, null);

  const section = target(undefined);
  const refresh = { target: section, request: { action: "/fragments/weather" } };
  listeners["htmx:before:request"]({ detail: { ctx: refresh } });
  listeners["htmx:error"]({ detail: { ctx: refresh } });
  assert.equal(section.content, null);

  // hx-on errors carry no request.
  listeners["htmx:error"]({ detail: {} });
});
