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

  // Setting the date
  const currentUTCDate = new Date();
  const fourHoursAgoUTCDate = new Date(currentUTCDate.getTime() - 14400000);
  const rfc3339TimeFourHoursAgo = fourHoursAgoUTCDate.toISOString();
  const startTime = document.getElementById("start");
  if (startTime) {
    startTime.value = rfc3339TimeFourHoursAgo;
  }

  const rfc3339TimeUTC = currentUTCDate.toISOString();
  const endTime = document.getElementById("end");
  if (endTime) {
    endTime.value = rfc3339TimeUTC;
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

  // Download todays files on initial load
  submitDownloadRequest(null);
}

async function submitDownloadRequest(event) {
  if (event !== null) {
    event.preventDefault();
  }
  try {
    const fileNames = await fetchFileNames();
    console.log(`Files to download: ${fileNames}`);
    await loadFiles(fileNames);
    console.log("Successfully download parquet files");
  } catch (error) {
    console.error("Error to download files:", error);
  }
}

function fetchFileNames() {
  const startTime = document.getElementById("start").value;
  const endTime = document.getElementById("end").value;
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
  const apiBase = window.API_BASE;
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
  const schemaDiv = document.getElementById(`${tableName}-schema`);
  if (!schemaDiv) return;

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
  schemaDiv.textContent = JSON.stringify(table_schema, null, 2);
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
}

function deleteTable(tableName) {
  const parentElement = document.getElementById(`${tableName}-container`);
  const childElement = document.getElementById(tableName);
  if (parentElement && childElement) {
    parentElement.removeChild(childElement);
  }
}

// Initialize when DOM is ready and on page navigation (HTMX)
document.addEventListener("DOMContentLoaded", initRawDataPage);
document.body.addEventListener("htmx:afterSwap", initRawDataPage);
