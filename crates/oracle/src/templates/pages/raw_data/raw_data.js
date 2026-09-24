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

function fetchFileNames() {
  // Get values from datetime-local inputs (format: YYYY-MM-DDTHH:MM)
  const startTimeRaw = document.getElementById("start").value;
  const endTimeRaw = document.getElementById("end").value;

  // Convert to RFC3339 format with seconds and Z suffix for API
  const startTime = startTimeRaw ? `${startTimeRaw}:00Z` : "";
  const endTime = endTimeRaw ? `${endTimeRaw}:00Z` : "";

  const forecasts = document.getElementById("forecasts").checked;
  const observations = document.getElementById("observations").checked;
  return new Promise((resolve, reject) => {
    let url = `/files?start=${startTime}&end=${endTime}&observations=${observations}&forecasts=${forecasts}`;
    console.log(`Requesting: ${url}`);
    fetch(url)
      .then((response) => {
        if (!response.ok) {
          throw new Error(`HTTP error! Status: ${response.status}`);
        }
        return response.json();
      })
      .then((data) => {
        console.log(data);
        resolve(data.file_names);
      })
      .catch((error) => {
        console.error("Error fetching file names:", error);
        reject(error);
      });
  });
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
  console.log(queryResult);
  const schemaTextarea = document.getElementById(`${tableName}-schema`);
  if (!schemaTextarea) return;

  const fields = {};
  for (const feild_index in queryResult.schema.fields) {
    const field = queryResult.schema.fields[feild_index];
    const column = queryResult.batches[0].data.children[feild_index];
    fields[field.name] = {};
    fields[field.name]["type"] = getType(column.values);
    fields[field.name]["nullable"] = field.nullable;
  }
  const table_schema = {
    table_name: tableName,
    fields: fields,
  };
  schemaTextarea.value = JSON.stringify(table_schema, null, 2);

  // Update status to show field count
  const fieldCount = Object.keys(fields).length;
  updateSchemaStatus(tableName, "loaded", fieldCount);
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

function loadTable(tableName, queryResult) {
  deleteErr();
  deleteTable(tableName);
  const tableParentDiv = document.getElementById(`${tableName}-container`);
  if (!tableParentDiv) return;

  const table = document.createElement("table");
  table.classList.add("table", "is-striped", "is-narrow", "is-bordered");
  table.id = tableName;

  const headerRow = table.createTHead().insertRow(0);
  for (const [index, column] of Object.entries(queryResult.schema.fields)) {
    const headerCell = headerRow.insertCell(index);
    headerCell.textContent = column.name;
  }

  for (const batch_index in queryResult.batches) {
    const row_count = queryResult.batches[batch_index].data.length;
    let data_grid = [];

    for (const column_index in queryResult.batches[batch_index].data.children) {
      const column =
        queryResult.batches[batch_index].data.children[column_index];
      let values = column.values;
      const array_type = getArrayType(values);

      if (array_type == "BigInt64Array") {
        values = formatInts(values);
      }
      if (array_type == "Uint8Array") {
        const offSets = column.valueOffsets;
        values = convertUintArrayToStrings(values, offSets);
      }
      data_grid.push(values);
    }

    for (let row_index = 0; row_index < row_count; row_index++) {
      const newRow = table.insertRow();
      for (const column_index in queryResult.batches[batch_index].data
        .children) {
        const cell = newRow.insertCell(column_index);
        cell.textContent = data_grid[column_index][row_index];
      }
    }

    tableParentDiv.appendChild(table);
  }

  // Enable download button when table is loaded
  const downloadButton = document.getElementById("downloadCsv");
  if (downloadButton) {
    downloadButton.disabled = false;
  }
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

function getArrayType(arr) {
  if (arr instanceof Uint8Array) return "Uint8Array";
  if (arr instanceof Float64Array) return "Float64Array";
  if (arr instanceof BigInt64Array) return "BigInt64Array";
  return "Unknown";
}

function getType(arr) {
  if (arr instanceof Uint8Array) return "Text";
  if (arr instanceof Float64Array) return "Float64";
  if (arr instanceof BigInt64Array) return "BigInt64";
  return "Unknown";
}

function convertUintArrayToStrings(uint8Array, valueOffsets) {
  const textDecoder = new TextDecoder("utf-8");
  const decodedStrings = [];

  for (let i = 0; i < valueOffsets.length; i++) {
    const start = i === 0 ? 0 : valueOffsets[i - 1];
    const end = valueOffsets[i];
    const stringBytes = uint8Array.subarray(start, end);
    const decodedString = textDecoder.decode(stringBytes);
    if (decodedString.length != 0) {
      decodedStrings.push(decodedString);
    }
  }
  return decodedStrings;
}

function formatInts(intArray) {
  const maxSafeInteger = BigInt(Number.MAX_SAFE_INTEGER);
  let formattedVals = [];
  for (let i = 0; i < intArray.length; i++) {
    if (intArray[i] > maxSafeInteger || intArray[i] < -maxSafeInteger) {
      formattedVals[i] = "NaN";
    } else {
      formattedVals[i] = `${intArray[i]}`;
    }
  }
  return formattedVals;
}

function clearQuerys(event) {
  deleteTable("queryResult");
  deleteErr();
  // Disable download button when clearing
  const downloadButton = document.getElementById("downloadCsv");
  if (downloadButton) {
    downloadButton.disabled = true;
  }
}

function downloadCsv() {
  const table = document.getElementById("queryResult");
  if (!table) return;

  let csv = [];

  // Get headers
  const headers = [];
  const headerRow = table.querySelector("thead tr");
  if (headerRow) {
    headerRow.querySelectorAll("th").forEach((th) => {
      headers.push(escapeCsvValue(th.textContent));
    });
    csv.push(headers.join(","));
  }

  // Get data rows
  const rows = table.querySelectorAll("tbody tr, tr:not(:first-child)");
  rows.forEach((row) => {
    const rowData = [];
    row.querySelectorAll("td").forEach((td) => {
      rowData.push(escapeCsvValue(td.textContent));
    });
    if (rowData.length > 0) {
      csv.push(rowData.join(","));
    }
  });

  // Create and download file
  const csvContent = csv.join("\n");
  const blob = new Blob([csvContent], { type: "text/csv;charset=utf-8;" });
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

function escapeCsvValue(value) {
  if (value === null || value === undefined) {
    return "";
  }
  let str = String(value);
  // Text starting with a formula character would run as a formula when the
  // CSV is opened in a spreadsheet; prefix it so it stays text.
  if (typeof value === "string" && /^[=+\-@\t\r]/.test(str)) {
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
