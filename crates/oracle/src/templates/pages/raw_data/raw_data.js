// Raw Data Page - DuckDB-based parquet file analyzer
// Only initializes when on the /raw page

let db = null;
let duckdb = null;

async function initRawDataPage() {
  // Only run on raw data page
  if (!document.getElementById("submit")) {
    return;
  }

  duckdb = window.duckdb;
  if (!duckdb) {
    console.error("DuckDB not loaded");
    return;
  }

  // Use API_BASE if available, otherwise use relative URLs
  window.API_BASE = window.API_BASE || "";

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

  const apiBase = window.API_BASE;
  console.log("api location:", apiBase);

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

  // Setting the date (4-hour window to avoid loading too much data)
  const currentUTCDate = new Date();
  const windowStartDate = new Date(
    currentUTCDate.getTime() - 4 * 60 * 60 * 1000,
  );

  // Format for datetime-local input (YYYY-MM-DDTHH:MM)
  const formatForInput = (date) => {
    return date.toISOString().slice(0, 16);
  };

  const startTime = document.getElementById("start");
  if (startTime) {
    startTime.value = formatForInput(windowStartDate);
  }

  const endTime = document.getElementById("end");
  if (endTime) {
    endTime.value = formatForInput(currentUTCDate);
  }

  const forecasts = document.getElementById("forecasts");
  if (forecasts) {
    forecasts.checked = true;
  }

  const observations = document.getElementById("observations");
  if (observations) {
    observations.checked = true;
  }

  const example_query = document.getElementById("customQuery");
  if (example_query) {
    example_query.value =
      "SELECT * FROM observations ORDER BY station_id, generated_at DESC LIMIT 200";
  }

  // Download files and run sample query on initial load
  submitDownloadRequest(null, true);
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
    console.log(`Files to download: ${fileNames}`);
    await loadFiles(fileNames);
    console.log("Successfully downloaded parquet files");

    // Hide loading states
    showSchemaLoading("forecasts", false);
    showSchemaLoading("observations", false);

    // Auto-run the sample query after initial load
    if (autoRunQuery) {
      await runQuery(null);
    }
  } catch (error) {
    console.error("Error downloading files:", error);
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
  const apiBase = window.API_BASE;

  return new Promise((resolve, reject) => {
    let url = `${apiBase}/files?start=${startTime}&end=${endTime}&observations=${observations}&forecasts=${forecasts}`;
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
  // Use absolute URL for DuckDB-WASM (it needs full URLs, not relative paths)
  const apiBase = window.API_BASE || window.location.origin;
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
    const res = await fetch(url);
    await db.registerFileBuffer(
      "buffer.parquet",
      new Uint8Array(await res.arrayBuffer()),
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
  const str = String(value);
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

// Example queries - these match the server-side queries that power the UI
// The server reads from parquet files; here the data is already loaded into
// the 'observations' and 'forecasts' tables by DuckDB-WASM.
const EXAMPLE_QUERIES = {
  daily_observations: `-- Daily observations: powers the weather map and dashboard
-- Groups hourly observations by station and day, classifies precipitation
-- using METAR weather codes, and derives humidity via the Magnus formula
WITH classified AS (
    SELECT *,
        CASE
            WHEN wx_string IS NOT NULL AND wx_string != '' THEN
                CASE
                    WHEN regexp_matches(wx_string, '(^|\\s)(SN|BLSN|DRSN)(\\s|$)') THEN 'snow'
                    WHEN regexp_matches(wx_string, '(^|\\s)(FZRA|FZDZ|PL|GR|GS|IC)(\\s|$)') THEN 'ice'
                    ELSE 'rain'
                END
            WHEN temperature_value IS NOT NULL AND temperature_value <= 2.0 THEN 'snow'
            ELSE 'rain'
        END AS precip_type
    FROM observations
)
SELECT
    station_id,
    DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT AS date,
    MIN(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_low,
    MAX(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_high,
    MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
    MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360) AS wind_direction,
    MAX(temperature_unit_code) AS temperature_unit_code,
    CASE
        WHEN AVG(dewpoint_value) IS NOT NULL AND AVG(temperature_value) IS NOT NULL
        THEN ROUND(100.0 * EXP((17.625 * AVG(dewpoint_value)) / (243.04 + AVG(dewpoint_value)))
             / EXP((17.625 * AVG(temperature_value)) / (243.04 + AVG(temperature_value))))::BIGINT
        ELSE NULL
    END AS humidity,
    SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'rain') AS rain_amt,
    SUM(precip_in * 10.0) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'snow') AS snow_amt,
    SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'ice') AS ice_amt
FROM classified
GROUP BY station_id, DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT
ORDER BY station_id, date`,

  daily_forecast: `-- Daily forecast summary: powers the forecast detail page
-- Deduplicates overlapping forecast windows (keeps latest generated_at),
-- then aggregates to daily granularity with rain/snow/ice separation
WITH deduped_forecasts AS (
    SELECT DISTINCT ON (station_id, begin_time, end_time)
        station_id, begin_time, end_time, min_temp, max_temp,
        wind_speed, wind_direction, relative_humidity_max, relative_humidity_min,
        temperature_unit_code, twelve_hour_probability_of_precipitation,
        liquid_precipitation_amt, snow_amt, snow_ratio, ice_amt, generated_at
    FROM forecasts
    ORDER BY station_id, begin_time, end_time, generated_at DESC
),
daily_forecasts AS (
    SELECT
        station_id,
        DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT AS date,
        MIN(begin_time) AS start_time,
        MAX(end_time) AS end_time,
        MIN(min_temp) FILTER (WHERE min_temp IS NOT NULL AND min_temp >= -200 AND min_temp <= 200) AS temp_low,
        MAX(max_temp) FILTER (WHERE max_temp IS NOT NULL AND max_temp >= -200 AND max_temp <= 200) AS temp_high,
        MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
        MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360) AS wind_direction,
        MAX(relative_humidity_max) FILTER (WHERE relative_humidity_max IS NOT NULL AND relative_humidity_max >= 0 AND relative_humidity_max <= 100) AS humidity_max,
        MIN(relative_humidity_min) FILTER (WHERE relative_humidity_min IS NOT NULL AND relative_humidity_min >= 0 AND relative_humidity_min <= 100) AS humidity_min,
        MAX(temperature_unit_code) AS temperature_unit_code,
        MAX(twelve_hour_probability_of_precipitation) FILTER (WHERE twelve_hour_probability_of_precipitation IS NOT NULL) AS precip_chance,
        SUM(liquid_precipitation_amt) FILTER (WHERE liquid_precipitation_amt IS NOT NULL AND liquid_precipitation_amt >= 0) AS total_qpf,
        SUM(snow_amt) FILTER (WHERE snow_amt IS NOT NULL AND snow_amt >= 0) AS snow_amt,
        AVG(snow_ratio) FILTER (WHERE snow_ratio IS NOT NULL AND snow_ratio > 0) AS avg_snow_ratio,
        SUM(ice_amt) FILTER (WHERE ice_amt IS NOT NULL AND ice_amt >= 0) AS ice_amt
    FROM deduped_forecasts
    GROUP BY station_id, DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT
)
SELECT
    station_id, date, MIN(start_time) AS start_time, MAX(end_time) AS end_time,
    MIN(temp_low) AS temp_low, MAX(temp_high) AS temp_high,
    MAX(wind_speed) AS wind_speed, MAX(wind_direction) AS wind_direction,
    MAX(humidity_max) AS humidity_max, MIN(humidity_min) AS humidity_min,
    MAX(temperature_unit_code) AS temperature_unit_code,
    MAX(precip_chance) AS precip_chance,
    GREATEST(0, COALESCE(
        SUM(total_qpf) - (SUM(snow_amt) / NULLIF(AVG(avg_snow_ratio), 0)) - COALESCE(SUM(ice_amt), 0),
        SUM(total_qpf) - COALESCE(SUM(ice_amt), 0)
    )) AS rain_amt,
    SUM(snow_amt) AS snow_amt,
    SUM(ice_amt) AS ice_amt
FROM daily_forecasts
GROUP BY station_id, date
ORDER BY station_id, date`,

  forecast_vs_observed: `-- Forecast vs Observed: compares forecast accuracy by joining
-- daily forecast aggregates with daily observation aggregates
WITH deduped_forecasts AS (
    SELECT DISTINCT ON (station_id, begin_time, end_time)
        station_id, begin_time, end_time, min_temp, max_temp, generated_at
    FROM forecasts
    ORDER BY station_id, begin_time, end_time, generated_at DESC
),
daily_fcst AS (
    SELECT
        station_id,
        DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT AS date,
        MIN(min_temp) FILTER (WHERE min_temp >= -200 AND min_temp <= 200) AS temp_low,
        MAX(max_temp) FILTER (WHERE max_temp >= -200 AND max_temp <= 200) AS temp_high
    FROM deduped_forecasts
    GROUP BY station_id, DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT
),
daily_obs AS (
    SELECT
        station_id,
        DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT AS date,
        MIN(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_low,
        MAX(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_high
    FROM observations
    GROUP BY station_id, DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT
)
SELECT
    f.station_id, f.date,
    f.temp_high AS forecast_high, f.temp_low AS forecast_low,
    o.temp_high AS observed_high, o.temp_low AS observed_low,
    f.temp_high - o.temp_high AS high_error,
    f.temp_low - o.temp_low AS low_error
FROM daily_fcst f
JOIN daily_obs o ON f.station_id = o.station_id AND f.date = o.date
ORDER BY f.station_id, f.date`,

  stations: `-- Station list: all unique stations with metadata
SELECT DISTINCT
    station_id,
    COALESCE(station_name, '') AS station_name,
    COALESCE(state, '') AS state,
    COALESCE(iata_id, '') AS iata_id,
    elevation_m, latitude, longitude
FROM observations
ORDER BY state, station_id`,
};

window.loadExampleQuery = function (name) {
  const query = EXAMPLE_QUERIES[name];
  if (!query) return;

  const textarea = document.getElementById("customQuery");
  if (textarea) {
    textarea.value = query;
  }

  // Auto-run the query
  runQuery(null);
};

// Initialize when DOM is ready and on page navigation (HTMX)
document.addEventListener("DOMContentLoaded", initRawDataPage);
document.body.addEventListener("htmx:afterSwap", initRawDataPage);
