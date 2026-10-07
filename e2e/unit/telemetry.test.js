const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.resolve(
  __dirname, "../../crates/oracle/src/templates/layouts/telemetry.js",
), "utf8");

// A page element: enough of closest/matches for the beacon's selectors.
function element(tag, { id = "", attributes = {}, text = "", parent = null } = {}) {
  return {
    tagName: tag.toUpperCase(),
    id,
    textContent: text,
    value: "typed by the reader",
    isContentEditable: false,
    parentElement: parent,
    getAttribute: (name) => (name in attributes ? attributes[name] : null),
    matches(selector) {
      return selector === "button, a, [role=button]"
        && (tag === "button" || tag === "a" || attributes.role === "button");
    },
    closest(selector) {
      for (let node = this; node; node = node.parentElement) {
        if (selector === '[data-telemetry="off"]') {
          if (node.getAttribute("data-telemetry") === "off") return node;
        } else if (node.matches("button, a, [role=button]")
          || node.getAttribute("data-track") !== null) {
          return node;
        }
      }
      return null;
    },
  };
}

function load({ on = true, rid = "0192f3a0-0000-7000-8000-0000000000aa", stored = {} } = {}) {
  const documentListeners = {};
  const windowListeners = {};
  const beacons = [];
  const storage = { ...stored };
  const meta = {
    'meta[name="telemetry"]': on ? { getAttribute: () => "on" } : null,
    'meta[name="request-id"]': rid ? { getAttribute: () => rid } : null,
  };
  const window = {
    addEventListener(name, listener) {
      windowListeners[name] = listener;
    },
  };
  vm.runInNewContext(source, {
    window,
    document: {
      visibilityState: "visible",
      referrer: "",
      querySelector: (selector) => meta[selector] || null,
      addEventListener(name, listener) {
        documentListeners[name] = listener;
      },
    },
    location: {
      href: "https://oracle.example.com/events?station=KORD",
      pathname: "/events",
      origin: "https://oracle.example.com",
    },
    navigator: {
      sendBeacon(url, blob) {
        beacons.push({ url, blob });
        return true;
      },
    },
    sessionStorage: {
      getItem: (key) => (key in storage ? storage[key] : null),
      setItem: (key, value) => {
        storage[key] = value;
      },
    },
    performance: { now: () => 1234.4, getEntriesByType: () => [] },
    crypto: globalThis.crypto,
    btoa,
    Blob,
    URL,
    WeakMap,
    setTimeout: (fn) => fn(),
    setInterval: () => 0,
  });
  return { documentListeners, windowListeners, beacons, storage, window };
}

async function sent(beacons) {
  return Promise.all(beacons.map(async ({ url, blob }) => {
    assert.equal(url, "/api/v1/telemetry");
    assert.equal(blob.type, "application/json");
    return JSON.parse(await blob.text());
  }));
}

test("without the telemetry meta tag the beacon does nothing", () => {
  const { documentListeners, windowListeners, storage, window } = load({ on: false });
  assert.deepEqual(Object.keys(documentListeners), []);
  assert.deepEqual(Object.keys(windowListeners), []);
  assert.deepEqual(storage, {});
  assert.equal(window.fdcMark, undefined);
});

test("the session id is 22 base64url characters kept for the tab", () => {
  const { storage } = load();
  assert.match(storage["fdc.sid"], /^[A-Za-z0-9_-]{22}$/);
  const again = load({ stored: { "fdc.sid": "AbCdEfGhIjKlMnOpQrStUv" } });
  assert.equal(again.storage["fdc.sid"], "AbCdEfGhIjKlMnOpQrStUv");
});

test("htmx requests carry the session id and are recorded with the reply's id", async () => {
  const { documentListeners, windowListeners, beacons, storage } = load();
  const ctx = { request: { method: "GET", action: "/fragments/weather?station=KORD", headers: {} } };
  documentListeners["htmx:config:request"]({ detail: { ctx } });
  assert.equal(ctx.request.headers["X-Session-Id"], storage["fdc.sid"]);
  documentListeners["htmx:before:request"]({ detail: { ctx } });
  ctx.response = {
    status: 200,
    headers: new Map([["X-Request-Id", "0192f3a0-0000-7000-8000-0000000000bb"]]),
  };
  documentListeners["htmx:finally:request"]({ detail: { ctx } });
  windowListeners.pagehide();

  const [batch] = await sent(beacons);
  assert.equal(batch.sid, storage["fdc.sid"]);
  assert.equal(batch.rid, "0192f3a0-0000-7000-8000-0000000000aa");
  assert.deepEqual(batch.events, [{
    ev: "htmx",
    t: 1234,
    page: "/events",
    verb: "GET",
    path: "/fragments/weather",
    status: 200,
    ms: 0,
    rid: "0192f3a0-0000-7000-8000-0000000000bb",
  }]);
});

test("clicks record buttons and links, never field values or marked areas", async () => {
  const { documentListeners, windowListeners, beacons } = load();
  const button = element("button", { id: "refresh", text: "  Refresh\n   " + "x".repeat(60) });
  documentListeners.click({ target: element("span", { parent: button }) });
  const tracked = element("div", { attributes: { "data-track": "map-pin" } });
  documentListeners.click({ target: tracked });
  documentListeners.click({ target: element("input") });
  const off = element("div", { attributes: { "data-telemetry": "off" } });
  documentListeners.click({ target: element("button", { text: "Secret", parent: off }) });
  documentListeners.submit({ target: element("form", { id: "search" }) });
  windowListeners.pagehide();

  const [batch] = await sent(beacons);
  const events = batch.events.map(({ t, page, ...fields }) => fields);
  assert.deepEqual(events, [
    { ev: "click", el: "button", id: "refresh", text: ("Refresh " + "x".repeat(60)).slice(0, 40) },
    { ev: "click", el: "div", track: "map-pin" },
    { ev: "submit", form: "search" },
  ]);
  assert.ok(!JSON.stringify(batch).includes("typed by the reader"));
});

test("errors and marks are queued, and batches hold at most 50 events", async () => {
  const { windowListeners, beacons, window } = load();
  windowListeners.error({
    message: "boom",
    filename: "https://oracle.example.com/assets/site.0123.js?v=1",
    lineno: 7,
  });
  for (let i = 0; i < 60; i++) window.fdcMark("step");
  windowListeners.pagehide();

  const batches = await sent(beacons);
  assert.deepEqual(batches.map((batch) => batch.events.length), [50, 11]);
  const { t, page, ...error } = batches[0].events[0];
  assert.deepEqual(error, { ev: "js_error", msg: "boom", src: "site.0123.js", line: 7 });
  assert.deepEqual(batches[1].events[10], { ev: "mark", t: 1234, page: "/events", name: "step" });
});

test("a broken handler never throws into the page", () => {
  const { documentListeners, windowListeners } = load();
  documentListeners["htmx:config:request"]({ detail: {} });
  documentListeners.click({ target: null });
  windowListeners.unhandledrejection({});
});
