const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.resolve(
  __dirname,
  "../../crates/oracle/src/templates/pages/raw_data/raw_data.js",
), "utf8");

// A small DOM boundary: rendering, schema/status updates and the real control selectors
// execute unchanged. The tests never replace the script's load/reset/query functions.
function element(tag, id = "", attributes = {}) {
  const classes = new Set();
  return {
    tag, id, attributes, children: [], listeners: {}, style: {}, dataset: {},
    value: "", textContent: "", checked: false, disabled: false,
    classList: {
      add: (...names) => names.forEach(name => classes.add(name)),
      remove: (...names) => names.forEach(name => classes.delete(name)),
      contains: name => classes.has(name),
    },
    hasAttribute(name) { return Object.hasOwn(this.attributes, name); },
    addEventListener(name, listener) { this.listeners[name] = listener; },
    appendChild(child) { this.children.push(child); return child; },
    removeChild(child) { this.children.splice(this.children.indexOf(child), 1); },
    createTHead() { return this.appendChild(element("thead")); },
    createTBody() { return this.appendChild(element("tbody")); },
    insertRow() { return this.appendChild(element("tr")); },
    insertCell() { return this.appendChild(element("td")); },
  };
}

function descendants(node) {
  return [node, ...node.children.flatMap(descendants)];
}

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function arrow(station, temperature) {
  return {
    schema: { fields: [
      { name: "station", type: "Utf8", nullable: false },
      { name: "temperature", type: "Float64", nullable: false },
    ] },
    numRows: 1,
    getChildAt(index) { return { get: () => [station, temperature][index] }; },
  };
}

function load() {
  const page = element("section", "raw-data");
  const body = element("body");
  body.appendChild(page);
  const add = (id, attrs = {}, tag = "button") => page.appendChild(element(tag, id, attrs));
  for (const id of ["submit", "runQuery"]) add(id, { "data-needs-db": "" });
  const example = add("example", { "data-query": "SELECT * FROM observations" });
  example.dataset.query = example.attributes["data-query"];
  example.closest = () => example;
  for (const id of ["clearQuery", "downloadCsv"]) add(id);
  for (const id of ["start", "end", "observations", "forecasts"]) add(id, {}, "input");
  for (const table of ["observations", "forecasts"]) {
    add(`${table}-schema`, {}, "textarea");
    add(`${table}-status`, {}, "span");
    add(`${table}-loading`, {}, "div");
  }
  add("customQuery", {}, "textarea").value = "SELECT * FROM observations";
  add("queryResult-container", {}, "div");
  add("raw-data-status", {}, "p");

  const document = {
    listeners: {}, body,
    getElementById: id => descendants(body).find(node => node.id === id) || null,
    createElement: tag => element(tag),
    addEventListener(name, listener) { this.listeners[name] = listener; },
    querySelectorAll(selector) {
      return descendants(page).filter(node => selector.split(",").some(part => {
        const simple = part.trim().replace(/^#raw-data\s+/, "");
        if (simple.startsWith("#")) return node.id === simple.slice(1);
        const attr = simple.match(/^\[([^\]]+)\]$/);
        return !!attr && node.hasAttribute(attr[1]);
      }));
    },
  };
  const get = id => document.getElementById(id);
  get("start").value = "2026-09-25T00:00";
  get("end").value = "2026-09-26T00:00";
  get("observations").checked = get("forecasts").checked = true;

  const hooks = {};
  const tables = new Map();
  const connections = [];
  const registered = [];
  const requests = [];
  const responses = [];
  const samples = { observations: arrow("KPWM", 64), forecasts: arrow("KPWM", 66) };
  const database = {
    async connect() {
      if (hooks.connect) await hooks.connect();
      const conn = {
        queries: [], closeCalls: 0,
        async query(sql) {
          this.queries.push(sql);
          if (hooks.query) await hooks.query(sql, this);
          if (/^DROP TABLE/i.test(sql)) {
            tables.delete("observations");
            tables.delete("forecasts");
            return;
          }
          const create = sql.match(/^CREATE OR REPLACE TABLE (observations|forecasts)/i);
          if (create) { tables.set(create[1], samples[create[1]]); return; }
          const select = sql.match(/^SELECT \* FROM (observations|forecasts)/i);
          if (select && tables.has(select[1])) return tables.get(select[1]);
          throw new Error("Table not found");
        },
        async close() {
          this.closeCalls++;
          if (hooks.close) await hooks.close(this);
        },
      };
      connections.push(conn);
      return conn;
    },
    async registerFileURL(name, url) {
      registered.push({ name, url });
      if (hooks.register) await hooks.register(name);
    },
  };
  const context = vm.createContext({
    document, URLSearchParams, window: { location: { origin: "https://oracle.test" } },
    console: { error() {} }, testDb: database,
    async fetch(url) {
      requests.push(url);
      if (!responses.length) throw new Error("Unexpected file request");
      const response = await responses.shift();
      if (response instanceof Error) throw response;
      return { ok: true, json: async () => ({ file_names: response }) };
    },
  });
  vm.runInContext(source, context);
  vm.runInContext("db = testDb; duckdb = { DuckDBDataProtocol: { HTTP: 1 } };", context);
  return {
    context, document, get, hooks, tables, connections, registered, requests, responses,
    loadFiles(files) { responses.push(files); return context.submitDownloadRequest(null); },
  };
}

const bothFiles = ["observations-2026-09-25.parquet", "forecasts-2026-09-25.parquet"];
const busyControls = ["submit", "runQuery", "example", "clearQuery", "start", "end", "observations", "forecasts"];

function assertClosed(harness) {
  assert.ok(harness.connections.length > 0);
  for (const conn of harness.connections) assert.equal(conn.closeCalls, 1, "each acquired connection is closed once");
}

function assertNoResult(harness) {
  assert.equal(harness.get("queryResult"), null);
  assert.equal(harness.get("downloadCsv").disabled, true, "stale CSV is unavailable");
}

test("an empty replacement clears prior tables, query rows, schemas, and CSV", async () => {
  const h = load();
  await h.loadFiles(bothFiles);
  await h.context.runQuery(null);
  assert.ok(h.get("queryResult"));
  assert.equal(h.get("downloadCsv").disabled, false);
  assert.equal(h.tables.size, 2);
  assert.match(h.get("forecasts-schema").value, /temperature/);

  await h.loadFiles([]);
  assert.equal(h.tables.size, 0);
  assertNoResult(h);
  for (const table of ["observations", "forecasts"]) {
    assert.equal(h.get(`${table}-schema`).value, "");
    assert.equal(h.get(`${table}-status`).textContent, "Empty");
    assert.equal(h.get(`${table}-loading`).style.display, "none");
  }
  assert.match(h.get("raw-data-status").textContent, /No files match/);
  // A new query cannot quietly recover rows from the previous date window.
  await h.context.runQuery(null);
  assertNoResult(h);
  assert.match(String(h.get("error").textContent), /Table not found/);
  assertClosed(h);
});

test("an unselected forecast table becomes Empty instead of retaining the previous schema", async () => {
  const h = load();
  await h.loadFiles(bothFiles);
  h.get("forecasts").checked = false;
  await h.loadFiles([bothFiles[0]]);
  assert.equal(h.tables.has("forecasts"), false);
  assert.equal(h.tables.has("observations"), true);
  assert.equal(h.get("forecasts-schema").value, "");
  assert.equal(h.get("forecasts-status").textContent, "Empty");
  assert.equal(h.get("observations-status").textContent, "2 fields");
  assert.equal(new URL(h.requests.at(-1), "https://oracle.test").searchParams.get("forecasts"), "false");
  assertClosed(h);
});

test("a partial download failure removes the already-created table and releases all connections", async () => {
  const h = load();
  await h.loadFiles(bothFiles);
  await h.context.runQuery(null);
  let partialTableExisted = false;
  h.hooks.query = async sql => {
    if (/^CREATE OR REPLACE TABLE forecasts/.test(sql)) {
      partialTableExisted = h.tables.has("observations");
      throw new Error("Broken forecast parquet");
    }
  };
  await h.loadFiles(bothFiles);
  assert.equal(partialTableExisted, true, "failure occurs after observations were loaded");
  assert.equal(h.tables.size, 0);
  assertNoResult(h);
  for (const table of ["observations", "forecasts"]) {
    assert.equal(h.get(`${table}-schema`).value, "");
    assert.equal(h.get(`${table}-status`).textContent, "Error");
  }
  assert.match(h.get("raw-data-status").textContent, /could not be loaded/);
  assert.equal(h.get("submit").disabled, false, "loading can be retried");
  assertClosed(h);
});

test("file registration failure closes its connection and clears the selection", async () => {
  const h = load();
  h.hooks.register = async () => { throw new Error("File unavailable"); };
  await h.loadFiles(bothFiles);
  assert.equal(h.tables.size, 0);
  assertNoResult(h);
  assertClosed(h);
});

test("failed cleanup disables querying stale tables until a successful new load", async () => {
  const h = load();
  await h.loadFiles(bothFiles);
  await h.context.runQuery(null);
  h.hooks.query = async sql => {
    if (/^DROP TABLE/.test(sql)) throw new Error("Database unavailable");
  };
  await h.context.submitDownloadRequest(null);
  assert.equal(h.tables.size, 2, "simulate a reset which cannot remove old tables");
  assertNoResult(h);
  assert.equal(h.get("runQuery").disabled, true);
  assert.equal(h.get("example").disabled, true);
  assert.equal(h.get("submit").disabled, false);
  assert.match(h.get("raw-data-status").textContent, /Reload this page before querying/);
  const count = h.connections.length;
  await h.context.runQuery(null);
  h.document.listeners.click({ target: h.get("example") });
  assert.equal(h.connections.length, count, "direct and example queries cannot bypass invalid-dataset state");

  h.hooks.query = null;
  await h.loadFiles([]);
  assert.equal(h.tables.size, 0);
  assert.equal(h.get("runQuery").disabled, false);
  assert.equal(h.get("example").disabled, false);
  assertClosed(h);
});

test("connection close failure cannot leave controls stuck busy", async () => {
  const h = load();
  await h.loadFiles(bothFiles);
  h.hooks.close = async () => { throw new Error("Worker close failed"); };
  await assert.rejects(h.context.runQuery(null), /Worker close failed/);
  for (const id of busyControls) assert.equal(h.get(id).disabled, false, id);
  assertClosed(h);
});

test("failed download connection close performs cleanup and permits retry", async () => {
  const h = load();
  h.hooks.close = async conn => {
    if (conn.queries.some(sql => /^CREATE OR REPLACE TABLE/.test(sql))) throw new Error("Worker close failed");
  };
  await h.loadFiles(bothFiles);
  assert.equal(h.tables.size, 0);
  assertNoResult(h);
  assert.match(h.get("raw-data-status").textContent, /could not be loaded/);
  assert.equal(h.get("submit").disabled, false);
  assertClosed(h);
});

test("a pending file request blocks overlapping loads, queries, Clear, and selection edits", async () => {
  const h = load();
  const pending = deferred();
  const loading = h.loadFiles(pending.promise);
  // resetDataset closes before the file request starts; wait for that observable boundary.
  for (let tries = 0; tries < 20 && !h.requests.length; tries++) {
    await new Promise(resolve => setImmediate(resolve));
  }
  assert.equal(h.requests.length, 1, "loading reaches the file request");
  for (const id of busyControls) assert.equal(h.get(id).disabled, true, id);
  assert.equal(h.get("downloadCsv").disabled, true);
  const acquired = h.connections.length;
  await h.context.submitDownloadRequest(null);
  await h.context.runQuery(null);
  h.document.listeners.click({ target: h.get("example") });
  assert.equal(h.requests.length, 1);
  assert.equal(h.connections.length, acquired);
  pending.resolve([]);
  await loading;
  for (const id of busyControls) assert.equal(h.get(id).disabled, false, id);
  assertNoResult(h);
  assertClosed(h);
});

test("a pending query blocks overlapping queries and dataset replacement", async () => {
  const h = load();
  await h.loadFiles(bothFiles);
  const entered = deferred();
  const pending = deferred();
  h.hooks.query = async sql => {
    if (sql === "SELECT * FROM observations") { entered.resolve(); await pending.promise; }
  };
  const querying = h.context.runQuery(null);
  await entered.promise;
  for (const id of busyControls) assert.equal(h.get(id).disabled, true, id);
  const acquired = h.connections.length;
  await h.context.runQuery(null);
  await h.context.submitDownloadRequest(null);
  assert.equal(h.connections.length, acquired);
  assert.equal(h.requests.length, 1);
  assertNoResult(h);
  pending.resolve();
  await querying;
  assert.ok(h.get("queryResult"));
  assert.equal(h.get("downloadCsv").disabled, false);
  for (const id of busyControls) assert.equal(h.get(id).disabled, false, id);
  assertClosed(h);
});
