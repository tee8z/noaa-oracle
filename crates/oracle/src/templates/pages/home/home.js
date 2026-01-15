const duckdb = window.duckdb;


// Setup duckdb
const JSDELIVR_BUNDLES = duckdb.getJsDelivrBundles();

const bundle = await duckdb.selectBundle(JSDELIVR_BUNDLES);
// Select a bundle based on browser checks
const worker_url = URL.createObjectURL(
    new Blob([`importScripts("${bundle.mainWorker}");`], { type: 'text/javascript' })
);

// Instantiate the asynchronus version of DuckDB-wasm
const worker = new Worker(worker_url);
const logger = new duckdb.ConsoleLogger();
const db = new duckdb.AsyncDuckDB(logger, worker);
await db.instantiate(bundle.mainModule, bundle.pthreadWorker);
URL.revokeObjectURL(worker_url);

const apiBase = API_BASE;
console.log("api location:", apiBase);


// Wire up buttons
const submitButton = document.getElementById('submit');
submitButton.addEventListener('click', submitDownloadRequest);

const queryButton = document.getElementById('runQuery');
queryButton.addEventListener('click', runQuery);

const clearButton = document.getElementById('clearQuery');
clearButton.addEventListener('click', clearQuerys);

// Setting the date
const currentUTCDate = new Date();
const fourHoursAgoUTCDate = new Date(currentUTCDate.getTime() - 14400000);
const rfc3339TimeFourHoursAgo = fourHoursAgoUTCDate.toISOString();
const startTime = document.getElementById('start');
startTime.value = rfc3339TimeFourHoursAgo;

const rfc3339TimeUTC = currentUTCDate.toISOString();
const endTime = document.getElementById('end');
endTime.value = rfc3339TimeUTC;

const forecasts = document.getElementById('forecasts');
forecasts.checked = true;

const observations = document.getElementById('observations');
observations.checked = true;

const example_query = document.getElementById('customQuery')
example_query.value = "SELECT * FROM observations ORDER BY station_id, generated_at DESC LIMIT 200"

// Download todays files on initial load
submitDownloadRequest(null);

async function submitDownloadRequest(event) {
    if (event !== null) { event.preventDefault() };
    try {
        const fileNames = await fetchFileNames();
        console.log(`Files to download: ${fileNames}`);
        await loadFiles(fileNames);
        console.log('Successfully download parquet files');
    } catch (error) {
        console.error('Error to download files:', error);
    }
}

function fetchFileNames() {
    const startTime = document.getElementById('start').value;
    const endTime = document.getElementById('end').value;
    const forecasts = document.getElementById('forecasts').checked;
    const observations = document.getElementById('observations').checked;

    return new Promise((resolve, reject) => {
        let url = `${apiBase}/files?start=${startTime}&end=${endTime}&observations=${observations}&forecasts=${forecasts}`;
        console.log(`Requesting: ${url}`)
        fetch(url)
            .then(response => {
                if (!response.ok) {
                    throw new Error(`HTTP error! Status: ${response.status}`);
                }
                return response.json();
            })
            .then(data => {
                console.log(data);
                resolve(data.file_names);
            })
            .catch(error => {
                console.error("Error fetching file names:", error)
                reject(error);
            });
    });
}

async function loadFiles(fileNames) {
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
        await db.registerFileURL(fileName, url, duckdb.DuckDBDataProtocol.HTTP, false);
        const res = await fetch(url);
        await db.registerFileBuffer('buffer.parquet', new Uint8Array(await res.arrayBuffer()));
    }
    if (Array.isArray(observation_files) && observation_files.length > 0) {
        await conn.query(`
        CREATE TABLE observations AS SELECT * FROM read_parquet(['${observation_files.join('\', \'')}'], union_by_name = true);
        `);
        const observations = await conn.query(`SELECT * FROM observations LIMIT 1;`);
        loadSchema("observations", observations);
    }

    if (Array.isArray(forecast_files) && forecast_files.length > 0) {
        let files =
            await conn.query(`
    CREATE TABLE forecasts AS SELECT * FROM read_parquet(['${forecast_files.join('\', \'')}'], union_by_name = true);
    `);
        const forecasts = await conn.query(`SELECT * FROM forecasts LIMIT 1;`);
        loadSchema("forecasts", forecasts);
    }
    await conn.close();
}

//TODO: limit to only SELECT statements
async function runQuery(event) {
    const rawQuery = document.getElementById('customQuery').value;
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
    const fields = {};
    for (const feild_index in queryResult.schema.fields) {
        const field = queryResult.schema.fields[feild_index];
        const column = queryResult.batches[0].data.children[feild_index];
        fields[field.name] = {};
        fields[field.name]['type'] = getType(column.values);
        fields[field.name]['nullable'] = field.nullable;
    }
    const table_schema = {
        "table_name": tableName,
        "fields": fields,
    }
    schemaDiv.textContent = JSON.stringify(table_schema, null, 2);
}

function loadTable(tableName, queryResult) {
    deleteErr();
    deleteTable(tableName);
    const tableParentDiv = document.getElementById(`${tableName}-container`);
    const table = document.createElement("table");
    table.classList.add("table");
    table.classList.add("is-striped");
    table.classList.add("is-narrow");
    table.classList.add("is-bordered");
    table.id = tableName;

    const headerRow = table.createTHead().insertRow(0);
    for (const [index, column] of Object.entries(queryResult.schema.fields)) {
        const headerCell = headerRow.insertCell(index);
        headerCell.textContent = column.name;
    }
    for (const batch_index in queryResult.batches) {
        const row_count = queryResult.batches[batch_index].data.length;
        console.log(row_count);
        let data_grid = [];
        for (const column_index in queryResult.batches[batch_index].data.children) {
            const column = queryResult.batches[batch_index].data.children[column_index];
            console.log(column);
            let values = column.values;
            const array_type = getArrayType(values);
            if (array_type == 'BigInt64Array') {
                values = formatInts(values);
            }
            if (array_type == 'Uint8Array') {
                const offSets = column.valueOffsets;
                values = convertUintArrayToStrings(values, offSets);
            }
            data_grid.push(values);
        }
        console.log(data_grid);
        for (let row_index = 0; row_index < row_count; row_index++) {
            const newRow = table.insertRow();
            for (const column_index in queryResult.batches[batch_index].data.children) {
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
    deleteErr();
    const errorDiv = document.createElement("div");
    errorDiv.id = 'error'
    errorDiv.textContent = err;
    errorDiv.classList.add("notification");
    errorDiv.classList.add("is-danger");
    errorDiv.classList.add("is-light");

    parentElement.appendChild(errorDiv);
}

function deleteErr() {
    const parentElement = document.getElementById(`queryResult-container`);
    const childElement = document.getElementById('error');

    // Check if the parent and child elements exist
    if (parentElement && childElement) {
        console.log("deleting from dom");
        // Remove the child element from the parent
        parentElement.removeChild(childElement);
    }
}

function getArrayType(arr) {
    if (arr instanceof Uint8Array) {
        return 'Uint8Array';
    } else if (arr instanceof Float64Array) {
        return 'Float64Array';
    } else if (arr instanceof BigInt64Array) {
        return 'BigInt64Array';
    } else {
        return 'Unknown';
    }
}

function getType(arr) {
    if (arr instanceof Uint8Array) {
        return 'Text';
    } else if (arr instanceof Float64Array) {
        return 'Float64';
    } else if (arr instanceof BigInt64Array) {
        return 'BigInt64';
    } else {
        return 'Unknown';
    }
}

function convertUintArrayToStrings(uint8Array, valueOffsets) {
    const textDecoder = new TextDecoder('utf-8');
    // Array to store the decoded strings
    const decodedStrings = [];

    for (let i = 0; i < valueOffsets.length; i++) {
        const start = (i === 0) ? 0 : valueOffsets[i - 1]; // Start position for the first string is 0
        const end = valueOffsets[i];
        const stringBytes = uint8Array.subarray(start, end);
        const decodedString = textDecoder.decode(stringBytes);
        if (decodedString.length != 0) {
            decodedStrings.push(decodedString);
        }
    }

    console.log(decodedStrings);
    return decodedStrings
}

function formatInts(intArray) {
    const maxSafeInteger = BigInt(Number.MAX_SAFE_INTEGER);
    let formattedVals = [];
    for (let i = 0; i < intArray.length; i++) {
        if (intArray[i] > maxSafeInteger || intArray[i] < -maxSafeInteger) {
            formattedVals[i] = "NaN";
        } else {
            formattedVals[i] = `${intArray[i]}`
        }
    }

    return formattedVals
}

function clearQuerys(event) {
    deleteTable('queryResult');
    deleteErr();
}

function deleteTable(tableName) {
    const parentElement = document.getElementById(`${tableName}-container`); // Replace with your actual parent element ID

    // Get a reference to the child element you want to delete
    const childElement = document.getElementById(tableName); // Replace with your actual child element ID

    // Check if the parent and child elements exist
    if (parentElement && childElement) {
        console.log("deleting from dom");
        // Remove the child element from the parent
        parentElement.removeChild(childElement);
    }
}


//TODO: add export to file ability from the tables