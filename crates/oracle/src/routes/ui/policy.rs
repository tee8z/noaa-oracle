//! The Content-Security-Policy for the UI's pages.
//!
//! Scripts load only from this site, as files: no inline scripts, `on…`
//! attributes or `eval`. htmx 4 has no setting that stops it evaluating
//! `hx-on` or running scripts in swapped HTML, so this policy is what stops
//! them. Pages also require Trusted Types: only the `htmx` policy
//! (`layouts/head.js`) may turn strings into HTML. The API and its docs are
//! not covered.

use axum::{
    extract::Request,
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

pub const PAGE_POLICY: &str = "script-src 'self'; object-src 'none'; base-uri 'none'; \
     frame-ancestors 'none'; form-action 'self'; \
     require-trusted-types-for 'script'; trusted-types htmx";

/// The raw data page also runs DuckDB-WASM from jsdelivr: the module and the
/// three modules it imports, the worker it starts from a `blob:` URL (which
/// loads DuckDB's worker script), and WebAssembly. DuckDB creates its worker
/// from a string, so this page does not require Trusted Types.
pub const RAW_DATA_POLICY: &str = "script-src 'self' 'wasm-unsafe-eval' \
     https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.29.0/ \
     https://cdn.jsdelivr.net/npm/apache-arrow@17.0.0/+esm \
     https://cdn.jsdelivr.net/npm/flatbuffers@24.3.25/+esm \
     https://cdn.jsdelivr.net/npm/tslib@2.6.3/+esm; \
     worker-src blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; \
     form-action 'self'";

/// Adds [`PAGE_POLICY`] to every UI response that doesn't set its own.
pub async fn content_security_policy(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(PAGE_POLICY),
        );
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
