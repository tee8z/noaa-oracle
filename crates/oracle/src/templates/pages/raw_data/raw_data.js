// Raw Data Page - DuckDB-based parquet file analyzer
// Only initializes when on the /raw page

// DuckDB-WASM is large, so only this page loads it, and only once.
const DUCKDB_MODULE =
  "https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.29.0/+esm";
let db = null;
let duckdb = null;

async function initRawDataPage() {
  // Only run on the raw data page, once per visit.
  const page = document.getElementById("raw-data");
  if (!page || page.dataset.ready) {
    return;
  }
  page.dataset.ready = "true";

  try {
    duckdb = duckdb || (await import(DUCKDB_MODULE));
  } catch (error) {
    console.error("DuckDB could not be loaded:", error);
    setStatus("DuckDB could not be loaded, so queries are unavailable.");
    return;
  }

  // Setup duckdb
  const JSDELIVR_BUNDLES = duckdb.getJsDelivrBundles();
  const bundle = await duckdb.selectBundle(JSDELIVR_BUNDLES);

  const worker_url = URL.createObjectURL(
    new Blob([`importScripts("${bundle.mainWorker}");`], {
      type: "text/javascript",
    }),
  );

  const worker = new Worker(worker_url);
  const logger = new duckdb.ConsoleLogger();
  db = new duckdb.AsyncDuckDB(logger, worker);
  await db.instantiate(bundle.mainModule, bundle.pthreadWorker);
  URL.revokeObjectURL(worker_url);

  // Wire up buttons
  const submitButton = document.getElementById("submit");
  if (submitButton) {
    submitButton.addEventListener("click", submitDownloadRequest);
  }

  const queryButton = document.getElementById("runQuery");
  if (queryButton) {
    queryButton.addEventListener("click", runQuery);
  }

  const clearButton = document.getElementById("clearQuery");
  if (clearButton) {
    clearButton.addEventListener("click", clearQuerys);
  }

  const downloadButton = document.getElementById("downloadCsv");
  if (downloadButton) {
    downloadButton.addEventListener("click", downloadCsv);
  }

  // Setup drag-to-scroll for query results
  setupDragScroll("queryResult-container");

  // Files are large (a day of forecasts is over 100 MB), so nothing
  // downloads until the reader asks.
  page.querySelectorAll("button[data-needs-db]").forEach((button) => {
    button.disabled = false;
  });
  setStatus("Ready. Choose a window and load the files.");
}

function setStatus(text) {
  const status = document.getElementById("raw-data-status");
  if (status) status.textContent = text;
}

async function submitDownloadRequest(event, autoRunQuery = false) {
  if (event !== null) {
    event.preventDefault();
  }
  try {
    // Show loading states
    showSchemaLoading("forecasts", true);
    showSchemaLoading("observations", true);

    const fileNames = await fetchFileNames();
    setStatus(`Loading ${fileNames.length} files…`);
    await loadFiles(fileNames);
    setStatus(`Loaded ${fileNames.length} files. Run a query below.`);

    // Hide loading states
    showSchemaLoading("forecasts", false);
    showSchemaLoading("observations", false);

    // Auto-run the sample query after initial load
    if (autoRunQuery) {
      await runQuery(null);
    }
  } catch (error) {
    console.error("Error downloading files:", error);
    setStatus("The files could not be loaded.");
    // Hide loading on error
    showSchemaLoading("forecasts", false);
    showSchemaLoading("observations", false);
    updateSchemaStatus("forecasts", "error");
    updateSchemaStatus("observations", "error");
  }
}

async function fetchFileNames() {
  // datetime-local values (YYYY-MM-DDTHH:MM) are UTC here; the API takes
  // RFC 3339.
  const utc = (id) => {
    const value = document.getElementById(id).value;
    return value ? `${value}:00Z` : "";
  };
  const query = new URLSearchParams({
    start: utc("start"),
    end: utc("end"),
    observations: document.getElementById("observations").checked,
    forecasts: document.getElementById("forecasts").checked,
  });
  const response = await fetch(`/files?${query}`);
  if (!response.ok) {
    throw new Error(`HTTP error! Status: ${response.status}`);
  }
  return (await response.json()).file_names;
}

async function loadFiles(fileNames) {
  // DuckDB-WASM needs absolute URLs, not relative paths.
  const apiBase = window.location.origin;
  const conn = await db.connect();
  let observation_files = [];
  let forecast_files = [];

  for (const fileName of fileNames) {
    let url = `${apiBase}/file/${fileName}`;
    if (fileName.includes("observations")) {
      observation_files.push(url);
    } else {
      forecast_files.push(url);
    }
    await db.registerFileURL(
      fileName,
      url,
      duckdb.DuckDBDataProtocol.HTTP,
      false,
    );
  }

  if (Array.isArray(observation_files) && observation_files.length > 0) {
    await conn.query(`
            CREATE OR REPLACE TABLE observations AS
            SELECT * FROM read_parquet(['${observation_files.join("', '")}'], union_by_name = true);
        `);
    const observations = await conn.query(
      `SELECT * FROM observations LIMIT 1;`,
    );
    loadSchema("observations", observations);
  }

  if (Array.isArray(forecast_files) && forecast_files.length > 0) {
    await conn.query(`
            CREATE OR REPLACE TABLE forecasts AS
            SELECT * FROM read_parquet(['${forecast_files.join("', '")}'], union_by_name = true);
        `);
    const forecasts = await conn.query(`SELECT * FROM forecasts LIMIT 1;`);
    loadSchema("forecasts", forecasts);
  }
  await conn.close();
}

async function runQuery(event) {
  const rawQuery = document.getElementById("customQuery").value;
  try {
    const conn = await db.connect();
    const queryResult = await conn.query(rawQuery);
    loadTable("queryResult", queryResult);
    await conn.close();
  } catch (error) {
    displayQueryErr(error);
  }
}

function loadSchema(tableName, queryResult) {
  const schemaTextarea = document.getElementById(`${tableName}-schema`);
  if (!schemaTextarea) return;

  const fields = {};
  for (const field of queryResult.schema.fields) {
    fields[field.name] = { type: String(field.type), nullable: field.nullable };
  }
  schemaTextarea.value = JSON.stringify({ table_name: tableName, fields }, null, 2);
  updateSchemaStatus(tableName, "loaded", queryResult.schema.fields.length);
}

// Schema UI helper functions
function showSchemaLoading(tableName, show) {
  const loadingDiv = document.getElementById(`${tableName}-loading`);
  const schemaTextarea = document.getElementById(`${tableName}-schema`);
  if (loadingDiv) {
    loadingDiv.style.display = show ? "flex" : "none";
  }
  if (schemaTextarea) {
    // Hide schema while loading, show when done
    if (show) {
      schemaTextarea.style.display = "none";
    } else {
      schemaTextarea.style.display = "block";
    }
  }

  // Update status while loading
  if (show) {
    updateSchemaStatus(tableName, "loading");
  }
}

function updateSchemaStatus(tableName, status, fieldCount = 0) {
  const statusTag = document.getElementById(`${tableName}-status`);
  if (!statusTag) return;

  statusTag.classList.remove(
    "is-light",
    "is-success",
    "is-warning",
    "is-danger",
  );

  if (status === "loaded") {
    statusTag.textContent = `${fieldCount} fields`;
    statusTag.classList.add("is-success");
  } else if (status === "loading") {
    statusTag.textContent = "Loading...";
    statusTag.classList.add("is-warning");
  } else if (status === "error") {
    statusTag.textContent = "Error";
    statusTag.classList.add("is-danger");
  } else {
    statusTag.textContent = "Empty";
    statusTag.classList.add("is-light");
  }
}

// The last query's column names, types and raw values, for the CSV.
let lastResult = null;

function loadTable(tableName, queryResult) {
  deleteErr();
  deleteTable(tableName);
  const tableParentDiv = document.getElementById(`${tableName}-container`);
  if (!tableParentDiv) return;

  const fields = queryResult.schema.fields;
  const types = fields.map((field) => String(field.type));
  const columns = fields.map((_, index) => queryResult.getChildAt(index));
  const rows = [];
  for (let row = 0; row < queryResult.numRows; row++) {
    rows.push(columns.map((column) => column.get(row)));
  }
  lastResult = { names: fields.map((field) => field.name), types, rows };

  const table = document.createElement("table");
  table.classList.add("table", "is-striped", "is-narrow", "is-bordered");
  table.id = tableName;
  const headerRow = table.createTHead().insertRow();
  for (const name of lastResult.names) {
    const header = document.createElement("th");
    header.textContent = name;
    headerRow.appendChild(header);
  }
  const body = table.createTBody();
  for (const values of rows) {
    const tableRow = body.insertRow();
    values.forEach((value, index) => {
      tableRow.insertCell().textContent = cellText(value, types[index]);
    });
  }
  tableParentDiv.appendChild(table);

  // Enable download button when table is loaded
  const downloadButton = document.getElementById("downloadCsv");
  if (downloadButton) {
    downloadButton.disabled = false;
  }
}

// A value as text: nulls empty, times in ISO 8601 UTC, large integers exact.
function cellText(value, type) {
  if (value === null || value === undefined) return "";
  if (value instanceof Date) return value.toISOString();
  if (typeof value === "number" && /^(Timestamp|Date)/.test(type)) {
    return new Date(value).toISOString();
  }
  return String(value);
}

function displayQueryErr(err) {
  console.error(err);
  const parentElement = document.getElementById(`queryResult-container`);
  if (!parentElement) return;

  deleteErr();
  const errorDiv = document.createElement("div");
  errorDiv.id = "error";
  errorDiv.textContent = err;
  errorDiv.classList.add("notification", "is-danger", "is-light");
  parentElement.appendChild(errorDiv);
}

function deleteErr() {
  const parentElement = document.getElementById(`queryResult-container`);
  const childElement = document.getElementById("error");
  if (parentElement && childElement) {
    parentElement.removeChild(childElement);
  }
}

function clearQuerys(event) {
  lastResult = null;
  deleteTable("queryResult");
  deleteErr();
  // Disable download button when clearing
  const downloadButton = document.getElementById("downloadCsv");
  if (downloadButton) {
    downloadButton.disabled = true;
  }
}

function downloadCsv() {
  if (!lastResult) return;
  const { names, types, rows } = lastResult;
  const csv = [names.map((name) => escapeCsvValue(name)).join(",")];
  for (const values of rows) {
    csv.push(values.map((value, index) => csvCell(value, types[index])).join(","));
  }

  // Create and download file
  const blob = new Blob([csv.join("\n")], { type: "text/csv;charset=utf-8;" });
  const link = document.createElement("a");
  const url = URL.createObjectURL(blob);

  link.setAttribute("href", url);
  link.setAttribute(
    "download",
    `query_result_${new Date().toISOString().slice(0, 19).replace(/:/g, "-")}.csv`,
  );
  link.style.visibility = "hidden";
  document.body.appendChild(link);
  link.click();
  document.body.removeChild(link);
  URL.revokeObjectURL(url);
}

function csvCell(value, type) {
  return typeof value === "string"
    ? escapeCsvValue(value)
    : escapeCsvValue(cellText(value, type), false);
}

// `text` marks values that were text in the result (and the column names).
function escapeCsvValue(value, text = true) {
  let str = String(value);
  // Text starting with a formula character would run as a formula when the
  // CSV is opened in a spreadsheet; prefix it so it stays text. Numbers,
  // negative ones included, are left alone.
  if (text && /^[=+\-@\t\r]/.test(str)) {
    str = "'" + str;
  }
  // Escape quotes and wrap in quotes if contains comma, quote, or newline
  if (str.includes(",") || str.includes('"') || str.includes("\n")) {
    return '"' + str.replace(/"/g, '""') + '"';
  }
  return str;
}

function deleteTable(tableName) {
  const parentElement = document.getElementById(`${tableName}-container`);
  const childElement = document.getElementById(tableName);
  if (parentElement && childElement) {
    parentElement.removeChild(childElement);
  }
}

function setupDragScroll(containerId) {
  const container = document.getElementById(containerId);
  if (!container) return;

  let isDown = false;
  let startX;
  let scrollLeft;

  container.addEventListener("mousedown", (e) => {
    // Only start drag if clicking on the container or table (not on interactive elements)
    if (
      e.target.tagName === "A" ||
      e.target.tagName === "BUTTON" ||
      e.target.tagName === "INPUT"
    ) {
      return;
    }
    isDown = true;
    container.classList.add("dragging");
    startX = e.pageX - container.offsetLeft;
    scrollLeft = container.scrollLeft;
    e.preventDefault();
  });

  container.addEventListener("mouseleave", () => {
    isDown = false;
    container.classList.remove("dragging");
  });

  container.addEventListener("mouseup", () => {
    isDown = false;
    container.classList.remove("dragging");
  });

  container.addEventListener("mousemove", (e) => {
    if (!isDown) return;
    e.preventDefault();
    const x = e.pageX - container.offsetLeft;
    const walk = (x - startX) * 1.5; // Multiply for faster scrolling
    container.scrollLeft = scrollLeft - walk;
  });
}

// Example query buttons carry their SQL in data-query (the server renders
// it from the .sql files beside this script).
document.addEventListener("click", function (event) {
  const example = event.target.closest("[data-query]");
  if (!example || !db) return;
  const textarea = document.getElementById("customQuery");
  if (textarea) textarea.value = example.dataset.query;
  runQuery(null);
});

// The page always loads as a whole document (see raw_data.rs).
document.addEventListener("DOMContentLoaded", initRawDataPage);
