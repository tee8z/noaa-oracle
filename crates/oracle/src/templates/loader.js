// Loader - imports external dependencies and loads app bundle
import * as duckdb from 'https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.29.0/+esm';

// Make DuckDB available globally for app code
window.duckdb = duckdb;

// Load the app bundle after dependencies are ready
import('/static/app.min.js').catch(err => {
    console.error('Failed to load app bundle:', err);
});
